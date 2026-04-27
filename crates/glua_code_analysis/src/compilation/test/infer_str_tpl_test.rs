#[cfg(test)]
mod test {
    use glua_parser::{LuaAstNode, LuaFuncStat, LuaIndexKey, LuaNameExpr, LuaVarExpr};
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{DiagnosticCode, LuaSignatureId, LuaType, LuaTypeDeclId, VirtualWorkspace};

    fn assert_union_contains_unknown_and(typ: LuaType, expected: LuaType) {
        let LuaType::Union(union) = typ else {
            panic!("expected union return type");
        };
        let types = union.into_vec();
        assert!(types.contains(&LuaType::Unknown), "{types:?}");
        assert!(types.contains(&expected), "{types:?}");
    }

    fn nth_name_expr_type_from_end(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
        nth_from_end: usize,
    ) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let root = semantic_model.get_root();
        let name_exprs = root
            .clone()
            .descendants::<LuaNameExpr>()
            .filter(|expr| expr.get_name_text().as_deref() == Some(name))
            .collect::<Vec<_>>();
        let name_expr = name_exprs
            .into_iter()
            .rev()
            .nth(nth_from_end)
            .expect("expected matching name expression");
        semantic_model
            .get_semantic_info(name_expr.syntax().clone().into())
            .expect("expected semantic info for name expression")
            .typ
    }

    fn signature_return_type_for_function(
        ws: &mut VirtualWorkspace,
        file_id: crate::FileId,
        name: &str,
    ) -> LuaType {
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let root = semantic_model.get_root();
        let func_stat = root
            .descendants::<LuaFuncStat>()
            .find(|stat| function_stat_name_is(stat, name))
            .expect("expected function declaration");
        let closure = func_stat.get_closure().expect("expected function closure");
        let signature_id = LuaSignatureId::from_closure(file_id, &closure);
        semantic_model
            .get_db()
            .get_signature_index()
            .get(&signature_id)
            .expect("expected function signature")
            .get_return_type()
    }

    fn function_stat_name_is(stat: &LuaFuncStat, name: &str) -> bool {
        match stat.get_func_name() {
            Some(LuaVarExpr::IndexExpr(index_expr)) => {
                matches!(index_expr.get_index_key(), Some(LuaIndexKey::Name(name_token)) if name_token.get_name_text() == name)
            }
            Some(LuaVarExpr::NameExpr(name_expr)) => {
                name_expr.get_name_text().as_deref() == Some(name)
            }
            _ => false,
        }
    }

    #[test]
    fn test_str_tpl_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
            ---@class aaa.xxx.bbb

            ---@generic T
            ---@param a aaa.`T`.bbb
            ---@return T
            function get_type(a)
            end
            "#,
        );

        let string_ty = ws.expr_ty("get_type('xxx')");
        let expected = ws.ty("aaa.xxx.bbb");
        assert_eq!(string_ty, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_returns_declared_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity
                ---@class sent_npc: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end
            "#,
        );

        ws.def(
            r#"
                ent = ents.Create('sent_custom')
            "#,
        );

        let result_ty = ws.expr_ty("ents.Create('sent_npc')");
        let expected = ws.ty("sent_npc");
        assert_eq!(result_ty, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_function_body_return_preserves_declared_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity
                ---@class glide_wheel: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                ENT = {}

                function ENT:CreateWheel()
                    local wheel = ents.Create("glide_wheel")
                    return wheel
                end
            "#,
        );

        let wheel_ty = ws.expr_ty("ENT:CreateWheel()");
        let expected = ws.ty("glide_wheel");
        assert_eq!(wheel_ty, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_function_body_return_is_not_erased_by_any_branch() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity
                ---@class glide_wheel: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                ---@return any
                function getExistingWheel()
                end

                ENT = {}

                function ENT:CreateWheel()
                    local existingWheel = getExistingWheel()
                    if IsValid(existingWheel) then
                        return existingWheel
                    end

                    local wheel = ents.Create("glide_wheel")
                    return wheel
                end
            "#,
        );

        let wheel_ty = ws.expr_ty("ENT:CreateWheel()");
        let expected = ws.ty("glide_wheel");
        assert_union_contains_unknown_and(wheel_ty, expected);
    }

    #[gtest]
    fn test_unresolved_return_fallback_uses_unknown_not_any() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                local existingWheel = otherWheel
                local otherWheel = existingWheel

                function CreateWheel()
                    return existingWheel
                end
            "#,
        );

        let wheel_ty = ws.expr_ty("CreateWheel()");
        assert_eq!(wheel_ty, LuaType::Unknown);
    }

    #[gtest]
    fn test_unknown_return_branch_is_preserved_before_precise_branch() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity
                ---@class glide_wheel: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                ---@return unknown
                function getExistingWheel()
                end

                ENT = {}

                function ENT:CreateWheel()
                    local existingWheel = getExistingWheel()
                    if existingWheel then
                        return existingWheel
                    end

                    local wheel = ents.Create("glide_wheel")
                    return wheel
                end
            "#,
        );

        let wheel_ty = ws.expr_ty("ENT:CreateWheel()");
        let expected = ws.ty("glide_wheel");
        assert_union_contains_unknown_and(wheel_ty, expected);
    }

    #[gtest]
    fn test_unknown_return_branch_is_preserved_after_precise_branch() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity
                ---@class glide_wheel: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                ---@return unknown
                function getFallbackWheel()
                end

                ENT = {}

                function ENT:CreateWheel()
                    if makeNewWheel then
                        local wheel = ents.Create("glide_wheel")
                        return wheel
                    end

                    return getFallbackWheel()
                end
            "#,
        );

        let wheel_ty = ws.expr_ty("ENT:CreateWheel()");
        let expected = ws.ty("glide_wheel");
        assert_union_contains_unknown_and(wheel_ty, expected);
    }

    #[gtest]
    fn test_shadowed_isvalid_does_not_narrow_function_body_return() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                local function IsValid(_)
                    return true
                end

                ---@param value string?
                function getValue(value)
                    if IsValid(value) then
                        return value
                    end

                    return "fallback"
                end
            "#,
        );

        let value_ty = ws.expr_ty("getValue(nil)");
        assert!(value_ty.is_nullable(), "{value_ty:?}");
    }

    #[gtest]
    fn test_table_field_existing_return_does_not_poison_precise_createwheel_return() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity
                ---@class base_glide: Entity
                ---@class glide_wheel: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                ENT = {}

                function ENT:CreateWheel()
                    local selfTbl = {}
                    ---@type table<number, glide_wheel>
                    selfTbl.wheels = {}
                    local index = 1
                    local existingWheel = selfTbl.wheels and selfTbl.wheels[index]

                    if IsValid(existingWheel) then
                        return existingWheel
                    end

                    local wheel = ents.Create("glide_wheel")
                    return wheel
                end
            "#,
        );

        let wheel_ty = ws.expr_ty("ENT:CreateWheel()");
        let expected = ws.ty("glide_wheel");
        assert_eq!(wheel_ty, expected);
    }

    #[gtest]
    fn test_gettable_existing_wheel_return_does_not_poison_precise_createwheel_return() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def_file(
            "lua/entities/base_glide/sv_wheels.lua",
            r#"
                ---@class Entity
                ---@return tableof<self>
                function Entity:GetTable()
                end

                ---@generic T : table
                ---@param metaName `T`
                ---@return T
                function FindMetaTable(metaName)
                end

                ---@class glide_wheel: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                local getTable = FindMetaTable("Entity").GetTable
                ENT = {}

                function ENT:Initialize()
                    local selfTbl = getTable(self)
                    --- @type table<number, glide_wheel>
                    selfTbl.wheels = {}
                    selfTbl.wheelCount = 0
                end

                function ENT:CreateWheel()
                    local selfTbl = getTable(self)
                    local index = selfTbl.wheelCount + 1
                    local existingWheel = selfTbl.wheels and selfTbl.wheels[index]

                    if IsValid(existingWheel) then
                        return existingWheel
                    end

                    local wheel = ents.Create("glide_wheel")
                    selfTbl.wheels[index] = wheel
                    return wheel
                end
            "#,
        );

        let expected = ws.ty("glide_wheel");
        let return_wheel_ty = nth_name_expr_type_from_end(&mut ws, file_id, "wheel", 0);
        assert_eq!(return_wheel_ty, expected);

        let signature_return = signature_return_type_for_function(&mut ws, file_id, "CreateWheel");
        assert_eq!(signature_return, expected);
    }

    #[gtest]
    fn test_unresolved_return_branch_is_not_dropped_for_precise_later_return() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def_file(
            "lua/entities/base_glide/sv_wheels.lua",
            r#"
                ---@class Entity
                ---@class glide_wheel: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                ENT = {}

                function ENT:CreateWheel()
                    if shouldReuseWheel then
                        return unresolvedWheel
                    end

                    local wheel = ents.Create("glide_wheel")
                    return wheel
                end
            "#,
        );

        let expected = LuaType::Ref(LuaTypeDeclId::global("glide_wheel"));
        let signature_return = signature_return_type_for_function(&mut ws, file_id, "CreateWheel");
        assert_union_contains_unknown_and(signature_return, expected);
    }

    #[gtest]
    fn test_direct_any_return_doc_preserves_user_authored_any_over_body_inference() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity
                ---@class glide_wheel: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                ---@return any
                function CreateWheel()
                    local wheel = ents.Create("glide_wheel")
                    return wheel
                end
            "#,
        );

        let wheel_ty = ws.expr_ty("CreateWheel()");
        assert_eq!(wheel_ty, LuaType::Any);
    }

    #[gtest]
    fn test_direct_any_return_doc_preserves_user_authored_any_over_unresolved_body() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@return any
                function getWheel()
                    return unresolvedWheel
                end
            "#,
        );

        let wheel_ty = ws.expr_ty("getWheel()");
        assert_eq!(wheel_ty, LuaType::Any);
    }

    #[gtest]
    fn test_any_parent_function_doc_does_not_override_precise_body_return() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity
                ---@class glide_wheel: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                ENT = {}

                ---@type fun(self: table): any
                ENT.CreateWheel = function(self)
                    local wheel = ents.Create("glide_wheel")
                    return wheel
                end
            "#,
        );

        let wheel_ty = ws.expr_ty("ENT.CreateWheel(ENT)");
        let expected = ws.ty("glide_wheel");
        assert_eq!(wheel_ty, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_auto_creates_missing_class_from_constraint() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end
            "#,
        );

        let result_ty = ws.expr_ty("ents.Create('sent_custom')");
        let expected = ws.ty("sent_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_local_alias_preserves_auto_created_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end
            "#,
        );

        ws.def(
            r#"
                local create_entity = ents.Create
                ent = create_entity('sent_custom')
            "#,
        );

        let result_ty = ws.expr_ty("ent");
        let expected = ws.ty("sent_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_member_alias_call_auto_creates_missing_class() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end
            "#,
        );

        ws.def(
            r#"
                local registry = {}
                registry.spawn = ents.Create
                ent = registry.spawn('sent_member_custom')
            "#,
        );

        let result_ty = ws.expr_ty("ent");
        let expected = ws.ty("sent_member_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_member_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_member_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_table_field_alias_call_auto_creates_missing_class() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end
            "#,
        );

        ws.def(
            r#"
                local registry = {
                    spawn = ents.Create,
                }
                ent = registry.spawn('sent_table_member_custom')
            "#,
        );

        let result_ty = ws.expr_ty("ent");
        let expected = ws.ty("sent_table_member_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_table_member_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_table_member_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_member_read_alias_call_auto_creates_missing_class() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end
            "#,
        );

        ws.def(
            r#"
                local registry = {}
                registry.spawn = ents.Create
                local alias = registry.spawn
                ent = alias('sent_member_read_custom')
            "#,
        );

        let result_ty = ws.expr_ty("ent");
        let expected = ws.ty("sent_member_read_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_member_read_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_member_read_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_wrapped_member_read_alias_call_auto_creates_missing_class() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
                ---@class Entity

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function meta(class)
                end
            "#,
        );

        ws.def(
            r#"
                local registry = {}
                registry.spawn = meta
                local alias = registry.spawn
                local wrapper = setmetatable({}, { __call = alias })
                ent = wrapper('sent_wrapped_member_read_custom')
            "#,
        );

        let result_ty = ws.expr_ty("ent");
        let expected = ws.ty("sent_wrapped_member_read_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_wrapped_member_read_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_wrapped_member_read_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_typed_member_call_auto_creates_missing_class() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                ---@class SpawnRegistry
                ---@field spawn fun<T: Entity>(class: `T`): T
            "#,
        );

        ws.def(
            r#"
                ---@type SpawnRegistry
                local registry

                ent = registry.spawn('sent_typed_member_custom')
            "#,
        );

        let result_ty = ws.expr_ty("ent");
        let expected = ws.ty("sent_typed_member_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_typed_member_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_typed_member_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_cross_file_local_alias_call_auto_creates_missing_class() {
        let mut ws = VirtualWorkspace::new();

        ws.def_files(vec![
            (
                "a_alias.lua",
                r#"
                    local create_entity = ents.Create
                    ent = create_entity('sent_cross_file_custom')
                "#,
            ),
            (
                "z_defs.lua",
                r#"
                    ---@class Entity

                    ents = {}

                    ---@generic T: Entity
                    ---@param class `T`
                    ---@return T
                    function ents.Create(class)
                    end
                "#,
            ),
        ]);

        let result_ty = ws.expr_ty("ent");
        let expected = ws.ty("sent_cross_file_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_cross_file_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_cross_file_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_wrapped_call_via_metatable_call_operator() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
                ---@class Entity

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function meta(class)
                end
            "#,
        );

        ws.def(
            r#"
                local wrapper = setmetatable({}, { __call = meta })
                ent = wrapper('sent_wrapped_custom')
            "#,
        );

        let result_ty = ws.expr_ty("ent");
        let expected = ws.ty("sent_wrapped_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_wrapped_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_wrapped_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_overload_only_signature_materializes_type() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity

                ---@generic T: Entity
                ---@overload fun(class: `T`): T
                function meta(class)
                end
            "#,
        );

        let result_ty = ws.expr_ty("meta('sent_overload_custom')");
        let expected = ws.ty("sent_overload_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_overload_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_overload_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_overload_only_wrapped_alias_materializes_type() {
        let mut ws = VirtualWorkspace::new_with_init_std_lib();

        ws.def(
            r#"
                ---@class Entity

                ---@generic T: Entity
                ---@overload fun(class: `T`): T
                function meta(class)
                end
            "#,
        );

        ws.def(
            r#"
                local alias = meta
                local wrapper = setmetatable({}, { __call = alias })
                ent = wrapper('sent_overload_wrapped_custom')
            "#,
        );

        let result_ty = ws.expr_ty("ent");
        let expected = ws.ty("sent_overload_wrapped_custom");
        assert_eq!(result_ty, expected);

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("sent_overload_wrapped_custom"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `sent_overload_wrapped_custom` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_str_tpl_generic_undefined_and_defined_class_paths() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@generic T : Entity
                ---@param class `T`
                ---@return T
                function ents_Create(class) end

                ---@class Entity
                local Entity = {}

                ---@class Player : Entity
                local Player = {}

                ---@return Player
                function Player_func() end

                ---@class my_entity : Entity

            "#,
        );

        ws.def(
            r#"
                ent = ents_Create("prop_physics")
                ply = Player_func()
                my_ent = ents_Create("my_entity")
            "#,
        );

        assert_eq!(
            ws.expr_ty("ents_Create(\"prop_physics\")"),
            ws.ty("prop_physics")
        );
        assert_eq!(ws.expr_ty("Player_func()"), ws.ty("Player"));
        assert_eq!(ws.expr_ty("ents_Create(\"my_entity\")"), ws.ty("my_entity"));

        let super_types: Vec<_> = ws
            .get_db_mut()
            .get_type_index()
            .get_super_types_iter(&LuaTypeDeclId::global("prop_physics"))
            .map(|iter| iter.cloned().collect())
            .unwrap_or_default();
        assert!(
            super_types.contains(&LuaType::Ref(LuaTypeDeclId::global("Entity"))),
            "expected `prop_physics` to inherit `Entity`, got {super_types:?}"
        );
    }

    #[gtest]
    fn test_user_full_scenario_type_and_field_resolution() {
        let mut ws = VirtualWorkspace::new();

        ws.def_file(
            "annotations.lua",
            r#"
                ---@class Entity
                local Entity = {}

                ---@class Player : Entity
                local Player = {}

                ---@class Panel
                local Panel = {}

                ---@class DPanel : Panel
                local DPanel = {}

                ---@generic T : Entity
                ---@param class `T`
                ---@return T
                function ents_Create(class) end

                ---@generic T : Panel
                ---@param classname `T`
                ---@return T
                function vgui_Create(classname) end

                ---@param playerIndex number
                ---@return Player
                function Player_func(playerIndex) end
            "#,
        );

        ws.enable_check(DiagnosticCode::UndefinedField);
        let scenario_file_id = ws.def_file(
            "scenario.lua",
            r#"
                local tbl = {}
                tbl.testVar = true

                local ent = ents_Create("prop_physics")
                ent.testVar = true

                local row = vgui_Create("DPanel")
                row.testVar = true

                local ply = Player_func(1)
                ply.testVar = true

                scenario_tbl = tbl
                scenario_ent = ent
                scenario_row = row
                scenario_ply = ply

                scenario_tbl_test_var = tbl.testVar
                scenario_ent_test_var = ent.testVar
                scenario_row_test_var = row.testVar
                scenario_ply_test_var = ply.testVar
            "#,
        );

        let diagnostics = ws
            .analysis
            .diagnose_file(scenario_file_id, CancellationToken::new())
            .unwrap_or_default();
        let undefined_field_code = Some(NumberOrString::String(
            DiagnosticCode::UndefinedField.get_name().to_string(),
        ));
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == undefined_field_code),
            "unexpected undefined-field diagnostics: {diagnostics:?}"
        );

        let tbl_type = ws.expr_ty("scenario_tbl");
        let table_type = ws.ty("table");
        assert!(ws.check_type(&tbl_type, &table_type));

        let ent_expected = ws.ty("prop_physics");
        let ent_type = ws.expr_ty("scenario_ent");
        assert_eq!(ent_type, ent_expected);

        let row_expected = ws.ty("DPanel");
        let row_type = ws.expr_ty("scenario_row");
        assert_eq!(row_type, row_expected);

        let ply_expected = ws.ty("Player");
        let ply_type = ws.expr_ty("scenario_ply");
        assert_eq!(ply_type, ply_expected);

        let bool_type = ws.ty("boolean");
        let tbl_field_type = ws.expr_ty("scenario_tbl_test_var");
        assert!(ws.check_type(&tbl_field_type, &bool_type));

        let ent_field_type = ws.expr_ty("scenario_ent_test_var");
        assert!(ws.check_type(&ent_field_type, &bool_type));

        let row_field_type = ws.expr_ty("scenario_row_test_var");
        assert!(ws.check_type(&row_field_type, &bool_type));

        let ply_field_type = ws.expr_ty("scenario_ply_test_var");
        assert!(ws.check_type(&ply_field_type, &bool_type));
    }
}
