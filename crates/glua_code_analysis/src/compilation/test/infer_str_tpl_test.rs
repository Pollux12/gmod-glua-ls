#[cfg(test)]
mod test {
    use glua_parser::{
        LuaAstNode, LuaAstToken, LuaFuncStat, LuaIndexKey, LuaLocalName, LuaNameExpr, LuaVarExpr,
    };
    use googletest::prelude::*;
    use lsp_types::NumberOrString;
    use tokio_util::sync::CancellationToken;

    use crate::{
        DiagnosticCode, Emmyrc, LuaSignatureId, LuaType, LuaTypeDeclId, LuaUnionType,
        VirtualWorkspace,
    };
    use smol_str::SmolStr;

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

    fn nth_local_name_type_from_end(
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
        let local_names = root
            .clone()
            .descendants::<LuaLocalName>()
            .filter(|local_name| local_name.get_text() == name)
            .collect::<Vec<_>>();
        let local_name = local_names
            .into_iter()
            .rev()
            .nth(nth_from_end)
            .expect("expected matching local name");
        let token = local_name
            .get_name_token()
            .expect("expected local name token");
        semantic_model
            .get_semantic_info(token.syntax().clone().into())
            .expect("expected semantic info for local name")
            .typ
    }

    fn nth_local_name_cached_type_from_end(
        ws: &VirtualWorkspace,
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
        let local_names = root
            .clone()
            .descendants::<LuaLocalName>()
            .filter(|local_name| local_name.get_text() == name)
            .collect::<Vec<_>>();
        let local_name = local_names
            .into_iter()
            .rev()
            .nth(nth_from_end)
            .expect("expected matching local name");
        let decl_id = crate::LuaDeclId::new(file_id, local_name.get_position());
        ws.analysis
            .compilation
            .get_db()
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|type_cache| type_cache.as_type().clone())
            .unwrap_or(LuaType::Unknown)
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
    fn test_str_tpl_generic_accepts_string_union_field() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def_file(
            "gamemodes/terrortown/entities/entities/ttt_random_weapon.lua",
            r#"
                ---@class Entity
                ---@class NULL: Entity
                ---@class item_ammo_smg1: Entity
                ---@class item_ammo_pistol: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T|NULL
                function ents.Create(class)
                end

                ---@class TttRandomWeapon
                ---@field AmmoEnt "item_ammo_smg1"|"item_ammo_pistol"

                ---@type TttRandomWeapon
                local ent

                function CreateAmmo()
                    local ammo = ents.Create(ent.AmmoEnt)
                    return ammo
                end
            "#,
        );

        let expected = ws.ty("item_ammo_smg1|item_ammo_pistol|NULL");
        let return_ammo_ty = nth_name_expr_type_from_end(&mut ws, file_id, "ammo", 0);
        assert_eq!(return_ammo_ty, expected);

        let signature_return = signature_return_type_for_function(&mut ws, file_id, "CreateAmmo");
        assert_eq!(signature_return, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_initial_indexing_materializes_string_union_field() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def_file(
            "gamemodes/terrortown/entities/entities/ttt_random_weapon.lua",
            r#"
                ---@class Entity
                ---@class NULL: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T|NULL
                function ents.Create(class)
                end

                ---@class TttRandomWeapon
                ---@field AmmoEnt "item_ammo_smg1"|"item_ammo_pistol"

                ---@type TttRandomWeapon
                local ent

                function CreateAmmo()
                    local ammo = ents.Create(ent.AmmoEnt)
                    return ammo
                end
            "#,
        );

        let expected = ws.ty("item_ammo_smg1|item_ammo_pistol|NULL");
        let return_ammo_ty = nth_name_expr_type_from_end(&mut ws, file_id, "ammo", 0);
        assert_eq!(return_ammo_ty, expected);

        let signature_return = signature_return_type_for_function(&mut ws, file_id, "CreateAmmo");
        assert_eq!(signature_return, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_initial_indexing_reproduces_ttt_random_weapon_chain() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        let file_ids = ws.def_files(vec![
            (
                "lua/autorun/ttt_test_annotations.lua",
                r#"
                ---@class Entity
                ---@class NULL: Entity
                ---@class Weapon: Entity

                ents = { TTT = {} }
                WEPS = {}
                weapons = {}
                math = {}
                table = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T|NULL
                function ents.Create(class)
                end

                ---@return (weapon_zm_pistol|weapon_zm_shotgun)[]
                function weapons.GetList()
                end

                ---@param t table
                ---@param value any
                function table.insert(t, value)
                end

                ---@param n integer
                ---@return integer
                function math.random(n)
                end

                ---@param value any
                ---@return TypeGuard<any>
                ---@return_cast value -NULL
                function IsValid(value)
                end
                "#,
            ),
            (
                "gamemodes/terrortown/entities/weapons/weapon_zm_pistol.lua",
                r#"
                SWEP.ClassName = "weapon_zm_pistol"
                SWEP.AutoSpawnable = true
                SWEP.AmmoEnt = "item_ammo_pistol_ttt"
                "#,
            ),
            (
                "gamemodes/terrortown/entities/weapons/weapon_zm_shotgun.lua",
                r#"
                SWEP.ClassName = "weapon_zm_shotgun"
                SWEP.AutoSpawnable = true
                SWEP.AmmoEnt = "item_box_buckshot_ttt"
                "#,
            ),
            (
                "gamemodes/terrortown/gamemode/weaponry_shd.lua",
                r#"
                function WEPS.IsEquipment(wep)
                    return false
                end

                function WEPS.GetClass(wep)
                    if istable(wep) then
                        return wep.ClassName or wep.Classname
                    elseif IsValid(wep) then
                        return wep:GetClass()
                    end
                end
                "#,
            ),
            (
                "gamemodes/terrortown/gamemode/ent_replace.lua",
                r#"
                local SpawnableSWEPs = nil
                function ents.TTT.GetSpawnableSWEPs()
                    if not SpawnableSWEPs then
                        local tbl = {}
                        for k, v in pairs(weapons.GetList()) do
                            if v and v.AutoSpawnable and (not WEPS.IsEquipment(v)) then
                                table.insert(tbl, v)
                            end
                        end

                        SpawnableSWEPs = tbl
                    end

                    return SpawnableSWEPs
                end

                function ents.TTT.GetFilteredSpawnableSWEPs(filter)
                    return ents.TTT.GetSpawnableSWEPs()
                end
                "#,
            ),
            (
                "gamemodes/terrortown/entities/entities/ttt_random_weapon.lua",
                r#"
                ENT.Type = "point"
                ENT.Base = "base_point"
                ENT.AutoAmmo = 0

                function ENT:Initialize()
                    local spawnflags = self:GetSpawnFlags()

                    local weps
                    if spawnflags != 0 then
                        weps = ents.TTT.GetFilteredSpawnableSWEPs(spawnflags)
                    else
                        weps = ents.TTT.GetSpawnableSWEPs()
                    end

                    if not weps then return end

                    local w = weps[math.random(#weps)]
                    local ent = ents.Create(WEPS.GetClass(w))
                    if IsValid(ent) then
                        local pos = self:GetPos()
                        if ent.AmmoEnt and self.AutoAmmo > 0 then
                            for i=1, self.AutoAmmo do
                                local ammo = ents.Create(ent.AmmoEnt)
                                print(ammo)
                                if IsValid(ammo) then
                                    ammo:SetPos(pos)
                                end
                            end
                        end
                    end
                end
                "#,
            ),
        ]);
        let file_id = *file_ids
            .iter()
            .find(|file_id| {
                ws.analysis
                    .compilation
                    .get_db()
                    .get_vfs()
                    .get_file_path(file_id)
                    .is_some_and(|path| path.ends_with("ttt_random_weapon.lua"))
            })
            .expect("expected random weapon file id");

        let expected = LuaType::from_vec(vec![
            LuaType::Ref(LuaTypeDeclId::global("item_ammo_pistol_ttt")),
            LuaType::Ref(LuaTypeDeclId::global("item_box_buckshot_ttt")),
            LuaType::Ref(LuaTypeDeclId::global("NULL")),
        ]);

        let later_ammo_ty = nth_name_expr_type_from_end(&mut ws, file_id, "ammo", 1);
        assert_eq!(later_ammo_ty, expected);

        let local_ammo_ty = nth_local_name_type_from_end(&mut ws, file_id, "ammo", 0);
        assert_eq!(local_ammo_ty, expected);

        let cached_ammo_ty = nth_local_name_cached_type_from_end(&ws, file_id, "ammo", 0);
        assert_eq!(cached_ammo_ty, expected);
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
    fn test_str_tpl_generic_binds_string_const_union() {
        let mut ws = VirtualWorkspace::new();

        let file_id = ws.def(
            r#"
                ---@class Entity
                ---@class widget_axis_arrow: Entity
                ---@class widget_axis_disc: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                local EntName = "widget_axis_arrow"
                if rotate then
                    EntName = "widget_axis_disc"
                end

                ent = ents.Create(EntName)
            "#,
        );

        let ent_name_ty = nth_name_expr_type_from_end(&mut ws, file_id, "EntName", 0);
        let expected_ent_name = LuaType::Union(
            LuaUnionType::from_vec(vec![
                LuaType::StringConst(SmolStr::new("widget_axis_arrow").into()),
                LuaType::StringConst(SmolStr::new("widget_axis_disc").into()),
            ])
            .into(),
        );
        assert_eq!(ent_name_ty, expected_ent_name);

        let ent_ty = ws.expr_ty("ent");
        let expected = ws.ty("widget_axis_arrow|widget_axis_disc");
        assert_eq!(ent_ty, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_override_with_extra_param_preserves_instantiated_return() {
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Entity
                ---@class letter: Entity

                ents = {}

                ---@generic T: Entity
                ---@param class `T`
                ---@return T
                function ents.Create(class)
                end

                local originalCreate = ents.Create
                function ents.Create(name, safety)
                    if originalCreate then
                        return originalCreate(name)
                    end
                    return nil
                end
            "#,
        );

        let spawned_ty = ws.expr_ty("ents.Create(\"letter\", \"meow\")");
        let expected = ws.ty("letter");
        assert_eq!(spawned_ty, expected);
    }

    #[gtest]
    fn test_str_tpl_generic_function_body_return_is_not_poisoned_by_any_branch() {
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
        assert_eq!(wheel_ty, expected);
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
    fn test_unknown_return_branch_does_not_poison_precise_later_branch() {
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
        assert_eq!(wheel_ty, expected);
    }

    #[gtest]
    fn test_unknown_return_branch_does_not_poison_precise_earlier_branch() {
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
        assert_eq!(wheel_ty, expected);
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
    fn test_unresolved_return_branch_does_not_poison_precise_later_return() {
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
        assert_eq!(signature_return, expected);
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

    // ── Inferred string default binding tests ───────────────────────────

    #[gtest]
    fn test_inferred_str_default_binds_str_tpl_generic() {
        // `panelClass = panelClass or "DScrollPanel"` then
        // `local p = fn(panelClass)` ⇒ p is DScrollPanel.
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DScrollPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                ---@param panelClass string|nil
                function foo(panelClass)
                    panelClass = panelClass or "DScrollPanel"
                    local p = create_panel(panelClass)
                    a = p
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let expected = ws.ty("DScrollPanel");
        assert_eq!(a_ty, expected);
    }

    #[gtest]
    fn test_no_or_default_does_not_bind_str_tpl() {
        // `---@param panelClass string` with NO or-default ⇒
        // `fn(panelClass)` ⇒ Panel (no binding).
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DScrollPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                ---@param panelClass string
                function foo(panelClass)
                    local p = create_panel(panelClass)
                    a = p
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let expected = ws.ty("Panel");
        assert_eq!(a_ty, expected);
    }

    #[gtest]
    fn test_non_self_or_does_not_bind_from_default_metadata() {
        // `local y = panelClass or "DScrollPanel"; fn(y)` ⇒
        // y is a different decl with no registered default ⇒ Panel.
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DScrollPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                ---@param panelClass string|nil
                function foo(panelClass)
                    local y = panelClass or "DScrollPanel"
                    local p = create_panel(y)
                    a = p
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let expected = ws.ty("Panel");
        assert_eq!(a_ty, expected);
    }

    #[gtest]
    fn test_inferred_str_default_binds_unannotated_variable() {
        // Without any annotation, `panelClass = panelClass or "DScrollPanel"`
        // still carries the inferred default and binds.
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DScrollPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                function foo(panelClass)
                    panelClass = panelClass or "DScrollPanel"
                    local p = create_panel(panelClass)
                    a = p
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let expected = ws.ty("DScrollPanel");
        assert_eq!(a_ty, expected);
    }

    // ── Flow-sensitive inferred default tests ──────────────────────────

    #[gtest]
    fn test_inferred_str_default_is_killed_by_later_reassignment() {
        // After `panelClass = panelClass or "DScrollPanel"` followed by
        // `panelClass = otherClass`, the default is dead at the use site.
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DScrollPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                ---@param panelClass string|nil
                ---@param otherClass string
                function AddTab(panelClass, otherClass)
                    panelClass = panelClass or "DScrollPanel"
                    panelClass = otherClass
                    local p = create_panel(panelClass)
                    a = p
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let expected = ws.ty("Panel");
        assert_eq!(a_ty, expected);
    }

    #[gtest]
    fn test_inferred_str_default_inside_conditional_does_not_bind() {
        // The default is inside a conditional — it does not dominate the use,
        // so it must NOT bind.
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DScrollPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                ---@param panelClass string|nil
                ---@param cond boolean
                function AddTab(panelClass, cond)
                    if cond then
                        panelClass = panelClass or "DScrollPanel"
                    end
                    local p = create_panel(panelClass)
                    a = p
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let expected = ws.ty("Panel");
        assert_eq!(a_ty, expected);
    }

    #[gtest]
    fn test_inferred_str_default_before_branch_still_binds() {
        // The default is before the branch and no reassignment happens,
        // so it MUST still bind.
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DScrollPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                ---@param panelClass string|nil
                ---@param cond boolean
                function AddTab(panelClass, cond)
                    panelClass = panelClass or "DScrollPanel"
                    if cond then
                        local x = 1
                    end
                    local p = create_panel(panelClass)
                    a = p
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let expected = ws.ty("DScrollPanel");
        assert_eq!(a_ty, expected);
    }

    #[gtest]
    fn test_inferred_str_default_branch_reassignment_kills_binding() {
        // Default before branch, but branch reassigns → default is dead.
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DScrollPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                ---@param panelClass string|nil
                ---@param cond boolean
                ---@param otherClass string
                function AddTab(panelClass, cond, otherClass)
                    panelClass = panelClass or "DScrollPanel"
                    if cond then
                        panelClass = otherClass
                    end
                    local p = create_panel(panelClass)
                    a = p
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let expected = ws.ty("Panel");
        assert_eq!(a_ty, expected);
    }

    #[gtest]
    fn test_self_coalescing_assignment_kills_explicit_default() {
        // When a function has `---@param panelClass string = "DPanel"` AND
        // then `panelClass = panelClass or "DScrollPanel"`,
        // the self-coalescing assignment kills the explicit default.
        // The inferred default "DScrollPanel" should bind downstream.
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DScrollPanel: Panel
                ---@class DPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                ---@param panelClass string|nil
                function foo(panelClass)
                    panelClass = panelClass or "DScrollPanel"
                    ---@cast panelClass string
                    local p = create_panel(panelClass)
                    a = p
                end

                ---@param panelClass string = "DPanel"
                function bar(panelClass)
                    panelClass = panelClass or "DScrollPanel"
                    local p = create_panel(panelClass)
                    b = p
                end
            "#,
        );

        // foo: no explicit default → inferred "DScrollPanel" binds
        let a_ty = ws.expr_ty("a");
        let expected_inferred = ws.ty("DScrollPanel");
        assert_eq!(a_ty, expected_inferred);

        // bar: self-coalescing assignment kills explicit default,
        // inferred "DScrollPanel" binds
        let b_ty = ws.expr_ty("b");
        let expected_inferred = ws.ty("DScrollPanel");
        assert_eq!(b_ty, expected_inferred);
    }

    // ── Explicit param default flow-validity tests ─────────────────────

    #[gtest]
    fn test_explicit_param_default_is_killed_by_reassignment() {
        // When a function parameter has `---@param panelClass string = "DPanel"`
        // but `panelClass = otherClass` reassigns it before the use site,
        // the explicit default must be killed. Expected: `p` is `Panel`.
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                ---@param panelClass string = "DPanel"
                ---@param otherClass string
                function AddTab(panelClass, otherClass)
                    panelClass = otherClass
                    local p = create_panel(panelClass)
                    a = p
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let expected = ws.ty("Panel");
        assert_eq!(a_ty, expected);
    }

    #[gtest]
    fn test_non_string_explicit_param_default_does_not_bind_str_tpl() {
        // When a function parameter has a non-string explicit default (e.g. boolean),
        // the string-template default resolver must NOT bind from it.
        // Expected: `p` is `Panel` (no binding from boolean default).
        // No reassignment — isolates the non-string-default behavior.
        let mut ws = VirtualWorkspace::new();

        ws.def(
            r#"
                ---@class Panel
                ---@class DPanel: Panel

                ---@generic T: Panel
                ---@param classname `T`
                ---@return T
                function create_panel(classname)
                end
            "#,
        );

        ws.def(
            r#"
                ---@param panelClass string = true
                function AddTab(panelClass)
                    local p = create_panel(panelClass)
                    a = p
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let expected = ws.ty("Panel");
        assert_eq!(a_ty, expected);
    }

    // ── VGUI panel reference inference tests ──────────────────────────

    /// Unwrap `Instance(...)` wrappers to get the base type.
    fn unwrap_instance(typ: &LuaType) -> &LuaType {
        match typ {
            LuaType::Instance(inst) => unwrap_instance(inst.get_base()),
            _ => typ,
        }
    }

    #[gtest]
    fn test_vgui_panel_ref_infers_class_from_enclosing_method() {
        // `PANEL:GenerateExample(ClassName)` with `vgui.Create(ClassName)` should
        // infer ClassName as the registered panel class name.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
                ---@class Panel
                ---@class DScrollPanel: Panel

                local PANEL = {}

                function PANEL:GenerateExample(ClassName, PropertySheet, Width, Height)
                    local ctrl = vgui.Create(ClassName)
                    a = ctrl
                end

                derma.DefineControl("DCategoryList", "", PANEL, "DScrollPanel")
            "#,
        );

        let a_ty = ws.expr_ty("a");
        // The return type is Instance-wrapped due to `---@return (instance) T`.
        // Unwrap to check the base panel class.
        let base = unwrap_instance(&a_ty).clone();
        let expected = ws.ty("DCategoryList");
        assert_eq!(base, expected);
    }

    #[gtest]
    fn test_panel_add_with_literal_infers_specific_class() {
        // `self:Add("DButton")` with a literal string should preserve the
        // literal class identity via the StrTplRef generic on Panel:Add.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
                ---@class Panel
                ---@class DButton: Panel

                ---@generic T: Panel
                ---@[call_arg("gmod.vgui_panel", "reference")]
                ---@param className `T`
                ---@return (instance) T
                function Panel:Add(className) end

                local PANEL = {}

                function PANEL:Init()
                    local child = self:Add("DButton")
                    a = child
                end

                derma.DefineControl("DCategoryList", "", PANEL, "Panel")
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let base = unwrap_instance(&a_ty).clone();
        let expected = ws.ty("DButton");
        assert_eq!(base, expected);
    }

    #[gtest]
    fn test_panel_add_with_panel_reference_param_infers_owning_class() {
        // `self:Add(ClassName)` should participate in the same contextual
        // VGUI reference inference as `vgui.Create(ClassName)` when the
        // class name flows in from a PANEL method parameter.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
                ---@class Panel

                ---@generic T: Panel
                ---@[call_arg("gmod.vgui_panel", "reference")]
                ---@param className `T`
                ---@return (instance) T
                function Panel:Add(className) end

                local PANEL = {}

                function PANEL:GenerateExample(ClassName)
                    a = self:Add(ClassName)
                end

                derma.DefineControl("DCategoryList", "", PANEL, "Panel")
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let base = unwrap_instance(&a_ty).clone();
        let expected = ws.ty("DCategoryList");
        assert_eq!(base, expected);
    }

    #[gtest]
    fn test_vgui_panel_ref_resolves_correct_panel_in_multi_panel_file() {
        // When a file has multiple `local PANEL = {}` blocks with separate
        // registrations, the inference must resolve to the correct panel
        // class for the enclosing method's PANEL — not just the first
        // registration in the file.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
                ---@class Panel

                local PANEL = {}
                derma.DefineControl("FirstPanel", "", PANEL, "Panel")

                local PANEL = {}
                function PANEL:GenerateExample(ClassName)
                    a = vgui.Create(ClassName)
                end
                derma.DefineControl("SecondPanel", "", PANEL, "Panel")
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let base = unwrap_instance(&a_ty).clone();
        let expected = ws.ty("SecondPanel");
        assert_eq!(base, expected);
    }

    #[gtest]
    fn test_vgui_panel_ref_resolves_correct_panel_in_reassigned_region() {
        // When a file reuses the same `local PANEL` declaration across multiple
        // registrations via plain reassignment, inference must pick the
        // registration region that encloses the method definition — not the
        // first registration using that declaration.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
                ---@class Panel

                local PANEL = {}
                derma.DefineControl("FirstPanel", "", PANEL, "Panel")

                PANEL = {}
                function PANEL:GenerateExample(ClassName)
                    a = vgui.Create(ClassName)
                end
                derma.DefineControl("SecondPanel", "", PANEL, "Panel")
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let base = unwrap_instance(&a_ty).clone();
        let expected = ws.ty("SecondPanel");
        assert_eq!(base, expected);
    }

    #[gtest]
    fn test_vgui_panel_ref_does_not_infer_for_non_panel_context() {
        // `---@param name string` with `vgui.Create(name)` in a non-panel
        // function should NOT infer a concrete panel class — falls back to Panel.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
                ---@class Panel

                ---@param name string
                function someFunction(name)
                    local ctrl = vgui.Create(name)
                    a = ctrl
                end
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let base = unwrap_instance(&a_ty).clone();
        let expected = ws.ty("Panel");
        assert_eq!(base, expected);
    }

    #[gtest]
    fn test_vgui_panel_ref_does_not_infer_for_nested_helper_param() {
        // A nested helper function parameter inside a PANEL method is not the
        // PANEL method parameter itself and must not inherit the owner-panel
        // contextual panel name.
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
                ---@class Panel

                local PANEL = {}

                function PANEL:Init()
                    local function make(className)
                        a = vgui.Create(className)
                    end
                end

                derma.DefineControl("DCategoryList", "", PANEL, "Panel")
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let base = unwrap_instance(&a_ty).clone();
        let expected = ws.ty("Panel");
        assert_eq!(base, expected);
    }

    #[gtest]
    fn test_vgui_panel_ref_preserves_literal_string_behavior() {
        // `vgui.Create("DCategoryList")` with a literal string should still
        // infer the specific panel class (existing behavior).
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_gmod_call_arg_builtins();

        ws.def(
            r#"
                ---@class Panel
                ---@class DCategoryList: Panel

                local ctrl = vgui.Create("DCategoryList")
                a = ctrl
            "#,
        );

        let a_ty = ws.expr_ty("a");
        let base = unwrap_instance(&a_ty).clone();
        let expected = ws.ty("DCategoryList");
        assert_eq!(base, expected);
    }
}
