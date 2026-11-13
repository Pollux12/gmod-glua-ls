#[cfg(test)]
mod base_class_test {
    use crate::{DiagnosticCode, LuaType, LuaTypeDeclId, VirtualWorkspace};

    const FACTORY: &str = r#"
        ---@class battlemechs.Base
        local META = {}

        ---@generic T : battlemechs.Base
        ---@[constructor("Initialize", "battlemechs.Base", false)]
        ---@param name battlemechs.`T`
        ---@[base_class]
        ---@param base (string|battlemechs.Base)?
        ---@return (definition) T
        function CreateMetaTable(name, base)
            ---@type (definition) battlemechs.Base
            local meta = setmetatable({}, META)
            return meta
        end
    "#;

    const MECH: &str = r#"
        local MECH = CreateMetaTable("Mech")
        function MECH:GetSpeed() return 1 end
    "#;

    fn factory_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();
        ws.def_file("factory.lua", FACTORY);
        ws
    }

    fn supers(ws: &VirtualWorkspace, name: &str) -> Vec<LuaType> {
        ws.analysis
            .compilation
            .get_db()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global(name))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default()
    }

    fn class_ref(name: &str) -> LuaType {
        LuaType::Ref(LuaTypeDeclId::global(name))
    }

    #[test]
    fn base_class_use_is_kept_on_the_base_param_only() {
        let ws = factory_workspace();
        let db = ws.analysis.compilation.get_db();
        let signature = db
            .get_signature_index()
            .iter()
            .map(|(_, signature)| signature)
            .find(|signature| {
                signature.param_docs.values().any(|doc| {
                    doc.get_attribute_by_name(crate::BASE_CLASS_ATTRIBUTE)
                        .is_some()
                })
            })
            .expect("expected CreateMetaTable signature");
        let tagged: Vec<_> = signature
            .param_docs
            .iter()
            .filter(|(_, doc)| {
                doc.get_attribute_by_name(crate::BASE_CLASS_ATTRIBUTE)
                    .is_some()
            })
            .map(|(idx, _)| *idx)
            .collect();
        assert_eq!(tagged, vec![1]);
    }

    #[test]
    fn named_base_becomes_a_supertype_and_members_resolve() {
        let mut ws = factory_workspace();
        ws.def_file("mech.lua", MECH);
        let w_use = r#"
            local w = CreateMetaTable("Walker", "Mech")
            print(w:GetSpeed())
        "#;
        assert!(ws.check_file_for(DiagnosticCode::UndefinedField, "walker.lua", w_use));
        assert!(supers(&ws, "battlemechs.Walker").contains(&class_ref("battlemechs.Mech")));
    }

    const OVERRIDES_BASE: &str = r#"
        ---@type battlemechs.Base
        local BASE
        ---@type integer
        BASE.Kind = 1
        ---@return integer
        function BASE:Describe() return 1 end
    "#;

    const OVERRIDES_MECH: &str = r#"
        local MECH = CreateMetaTable("Mech")
        ---@type string
        MECH.Kind = "mech"
        ---@return string
        function MECH:Describe() return "mech" end
    "#;

    const OVERRIDES_WALKER: &str = r#"
        local walker = CreateMetaTable("Walker", "Mech")
        WalkerKind = walker.Kind
        WalkerDescription = walker:Describe()
    "#;

    fn assert_walker_uses_mech_overrides(ws: &mut VirtualWorkspace) {
        assert_eq!(
            supers(ws, "battlemechs.Walker"),
            vec![class_ref("battlemechs.Mech")]
        );
        let kind = ws.expr_ty("WalkerKind");
        assert_eq!(ws.humanize_type(kind), "string");
        let description = ws.expr_ty("WalkerDescription");
        assert_eq!(ws.humanize_type(description), "string");
    }

    #[test]
    fn the_named_parent_overrides_the_root_class_members() {
        let mut ws = factory_workspace();
        ws.def_file("base.lua", OVERRIDES_BASE);
        ws.def_file("mech.lua", OVERRIDES_MECH);
        ws.def_file("walker.lua", OVERRIDES_WALKER);
        assert_walker_uses_mech_overrides(&mut ws);
    }

    #[test]
    fn a_named_parent_from_a_later_file_overrides_the_root_class_members() {
        let mut ws = factory_workspace();
        ws.def_file("base.lua", OVERRIDES_BASE);
        ws.def_file("walker.lua", OVERRIDES_WALKER);
        ws.def_file("mech.lua", OVERRIDES_MECH);
        assert_walker_uses_mech_overrides(&mut ws);
    }

    #[test]
    fn a_base_passed_as_a_class_value_becomes_a_supertype() {
        let mut ws = factory_workspace();
        let w_use = r#"
            local MECH = CreateMetaTable("Mech")
            function MECH:GetSpeed() return 1 end

            local w = CreateMetaTable("Walker", MECH)
            print(w:GetSpeed())
        "#;
        assert!(ws.check_file_for(DiagnosticCode::UndefinedField, "walker.lua", w_use));
        assert!(ws.check_file_for(DiagnosticCode::ParamTypeMismatch, "walker.lua", w_use));
        assert!(supers(&ws, "battlemechs.Walker").contains(&class_ref("battlemechs.Mech")));
    }

    #[test]
    fn a_class_value_base_from_a_later_file_becomes_a_supertype() {
        let mut ws = factory_workspace();
        ws.def_file(
            "walker.lua",
            r#"
                local w = CreateMetaTable("Walker", battlemechs_mech)
            "#,
        );
        ws.def_file(
            "mech.lua",
            r#"
                battlemechs_mech = CreateMetaTable("Mech")
            "#,
        );
        assert!(supers(&ws, "battlemechs.Walker").contains(&class_ref("battlemechs.Mech")));
    }

    #[test]
    fn missing_base_inherits_only_the_root_class() {
        let mut ws = factory_workspace();
        ws.def_file("mech.lua", MECH);
        ws.def_file("arf.lua", r#"local arf = CreateMetaTable("Arf")"#);
        assert_eq!(
            supers(&ws, "battlemechs.Arf"),
            vec![class_ref("battlemechs.Base")]
        );
    }

    #[test]
    fn unknown_base_adds_no_edge_and_no_class() {
        let mut ws = factory_workspace();
        ws.def_file("x.lua", r#"local x = CreateMetaTable("X", "Nope")"#);
        assert_eq!(
            supers(&ws, "battlemechs.X"),
            vec![class_ref("battlemechs.Base")]
        );
        assert!(
            ws.analysis
                .compilation
                .get_db()
                .get_type_index()
                .get_type_decl(&LuaTypeDeclId::global("battlemechs.Nope"))
                .is_none()
        );
    }

    #[test]
    fn non_constant_base_adds_no_edge() {
        let mut ws = factory_workspace();
        ws.def_file("mech.lua", MECH);
        ws.def_file(
            "w.lua",
            r#"
                local b = math.random() > 0.5 and "Mech" or nil
                local w = CreateMetaTable("Walker", b)
            "#,
        );
        assert_eq!(
            supers(&ws, "battlemechs.Walker"),
            vec![class_ref("battlemechs.Base")]
        );
    }

    #[test]
    fn self_and_cyclic_bases_add_no_edge() {
        let mut ws = factory_workspace();
        ws.def_file(
            "cycle.lua",
            r#"
                local a = CreateMetaTable("A", "A")
                local b = CreateMetaTable("B", "C")
                local c = CreateMetaTable("C", "B")
            "#,
        );
        assert!(!supers(&ws, "battlemechs.A").contains(&class_ref("battlemechs.A")));
        let b_over_c = supers(&ws, "battlemechs.B").contains(&class_ref("battlemechs.C"));
        let c_over_b = supers(&ws, "battlemechs.C").contains(&class_ref("battlemechs.B"));
        assert!(!(b_over_c && c_over_b));
    }

    fn defs_with_attributes() -> String {
        format!(
            "---@attribute constructor(name: string, root_class: string?, strip_self: boolean?, return_self: boolean?)\n\
             ---@attribute base_class()\n{FACTORY}"
        )
    }

    #[test]
    fn removing_the_base_argument_matches_a_cold_build() {
        let defs = defs_with_attributes();
        let ws = VirtualWorkspace::assert_warm_matches_cold(
            |_| {},
            &[
                ("factory.lua", defs.as_str()),
                ("mech.lua", MECH),
                ("w.lua", r#"local w = CreateMetaTable("Walker", "Mech")"#),
            ],
            &[("w.lua", r#"local w = CreateMetaTable("Walker")"#)],
        );
        assert!(!supers(&ws, "battlemechs.Walker").contains(&class_ref("battlemechs.Mech")));
    }

    #[test]
    fn deleting_the_base_file_matches_a_cold_build() {
        let defs = defs_with_attributes();
        let ws = VirtualWorkspace::assert_warm_matches_cold(
            |_| {},
            &[
                ("factory.lua", defs.as_str()),
                ("mech.lua", MECH),
                ("w.lua", r#"local w = CreateMetaTable("Walker", "Mech")"#),
            ],
            &[("mech.lua", "")],
        );
        assert!(!supers(&ws, "battlemechs.Walker").contains(&class_ref("battlemechs.Mech")));
    }
}
