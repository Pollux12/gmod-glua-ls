#![cfg(test)]

use crate::{DiagnosticCode, FileId, LuaType, VirtualWorkspace};
use glua_parser::{LuaAstNode, LuaCallExpr, LuaExpr};
use tokio_util::sync::CancellationToken;

const API: &str = include_str!("fixtures/self_call_type/api.lua");
const WEAPONS: &str = "gamemodes/testgm/entities/weapons";

fn workspace() -> VirtualWorkspace {
    let mut ws = VirtualWorkspace::new_with_init_std_lib();
    ws.def_gmod_call_arg_builtins();
    ws.def_file("lua/annotations/self_call_type_api.lua", API);
    ws
}

fn weapon(ws: &mut VirtualWorkspace, path: &str, text: &str) -> FileId {
    ws.def_file(&format!("{WEAPONS}/{path}"), text)
}

/// The type of the `occurrence`-th `self:GetOwner()` call of `id`, in source order, a union rendered
/// arm by arm.
fn owner_type(ws: &VirtualWorkspace, id: FileId, occurrence: usize) -> String {
    let model = ws.analysis.compilation.get_semantic_model(id).unwrap();
    let call = model
        .get_root()
        .descendants::<LuaCallExpr>()
        .filter(|call| call.syntax().text() == "self:GetOwner()")
        .nth(occurrence)
        .unwrap();
    match model.infer_expr(LuaExpr::CallExpr(call)).unwrap() {
        LuaType::Union(union) => {
            let arms = union
                .into_vec()
                .into_iter()
                .map(|arm| ws.humanize_type(arm))
                .collect::<Vec<_>>();
            format!("({})", arms.join("|"))
        }
        typ => ws.humanize_type(typ),
    }
}

/// The arms of a rendered type, sorted: `(NPC|Player)` gives `["NPC", "Player"]`.
fn arms(rendered: &str) -> Vec<String> {
    let mut arms = rendered
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split('|')
        .map(str::to_string)
        .collect::<Vec<_>>();
    arms.sort();
    arms
}

fn diagnostics(ws: &mut VirtualWorkspace, id: FileId, code: DiagnosticCode) -> usize {
    ws.analysis.diagnostic.enable_only(code);
    ws.analysis
        .diagnose_file(id, CancellationToken::new())
        .unwrap()
        .len()
}

#[test]
fn a_player_only_hook_types_its_owner_as_player() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think() self:GetOwner():DrawViewModel(false) end
function SWEP:Deploy() self:GetOwner():DrawViewModel(false) return true end",
    );
    assert_eq!(owner_type(&ws, id, 0), "Player");
    assert_eq!(owner_type(&ws, id, 1), "Player");
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        0
    );
}

#[test]
fn a_hook_without_a_contract_keeps_the_declared_owner() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Holster() local owner = self:GetOwner() return true end
function SWEP:Unreached() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, id, 0), "(Entity|NULL)");
    assert_eq!(owner_type(&ws, id, 1), "(Entity|NULL)");
}

#[test]
fn a_deferred_function_does_not_inherit_the_contract() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think() timer.Simple(0, function() local owner = self:GetOwner() end) end",
    );
    assert_eq!(owner_type(&ws, id, 0), "(Entity|NULL)");
}

#[test]
fn a_shadowed_self_does_not_inherit_the_contract() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "---@param other Weapon
function SWEP:Think(other) local self = other local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, id, 0), "(Entity|NULL)");
}

#[test]
fn a_method_called_on_self_from_a_hook_takes_its_contract() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think() self:Zoom() end
function SWEP:Zoom() local owner = self:GetOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Player"]);
}

#[test]
fn the_npc_arm_of_a_contract_holds_only_in_its_realm() {
    let mut ws = workspace();
    let server = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "function SWEP:PrimaryAttack() local owner = self:GetOwner() end",
    );
    let client = weapon(
        &mut ws,
        "weapon_pistol/cl_init.lua",
        "function SWEP:PrimaryAttack() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, server, 0), "(NPC|Player)");
    assert_eq!(owner_type(&ws, client, 0), "Player");
}

#[test]
fn a_method_reached_from_an_npc_entry_takes_the_npc_arm() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_dryfire/shared.lua",
        "function SWEP:PrimaryAttack() if not self:CanPrimaryAttack() then return end end
function SWEP:CanPrimaryAttack() self:Reload() return false end
function SWEP:Reload() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, id, 0), "(NPC|Player)");
}

#[test]
fn a_derived_class_resolves_the_calls_its_base_makes_on_self() {
    let mut ws = workspace();
    weapon(
        &mut ws,
        "weapon_dryfire_base/shared.lua",
        "function SWEP:PrimaryAttack() if not self:CanPrimaryAttack() then return end end
function SWEP:CanPrimaryAttack() self:Reload() return false end
function SWEP:SecondaryAttack() self:CanSecondaryAttack() end
function SWEP:CanSecondaryAttack() self:DryFire() return false end
function SWEP:DryFire() self:Reload() end
function SWEP:Reload() end",
    );
    let inherits_dry_fire = weapon(
        &mut ws,
        "weapon_rifle/shared.lua",
        "SWEP.Base = \"weapon_dryfire_base\"
function SWEP:Reload() local owner = self:GetOwner() end",
    );
    let overrides_dry_fire = weapon(
        &mut ws,
        "weapon_shotgun/shared.lua",
        "SWEP.Base = \"weapon_dryfire_base\"
function SWEP:CanPrimaryAttack() return true end
function SWEP:Reload() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, inherits_dry_fire, 0), "(NPC|Player)");
    assert_eq!(owner_type(&ws, overrides_dry_fire, 0), "(NPC|Player)");
}

#[test]
fn an_entity_hook_of_the_same_name_has_no_owner_contract() {
    let mut ws = workspace();
    let id = ws.def_file(
        "gamemodes/testgm/entities/entities/ent_lamp/shared.lua",
        "function ENT:Think() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, id, 0), "(Entity|NULL)");
}

#[test]
fn a_member_only_some_owner_classes_have_reports_the_classes_that_do() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "function SWEP:PrimaryAttack() self:GetOwner():LagCompensation(true) end",
    );
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
}

#[test]
fn a_member_every_owner_class_has_is_valid() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "function SWEP:PrimaryAttack() local src = self:GetOwner():GetShootPos() end",
    );
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        0
    );
}

#[test]
fn a_local_owner_reports_its_first_partial_member_once() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "function SWEP:PrimaryAttack()
    local owner = self:GetOwner()
    owner:LagCompensation(true)
    owner:GetShootPos()
    owner:LagCompensation(false)
end",
    );
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
}

#[test]
fn excluding_one_owner_class_narrows_to_the_other() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "function SWEP:PrimaryAttack()
    local owner = self:GetOwner()
    if owner:IsNPC() then return end
    owner:LagCompensation(true)
end",
    );
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        0
    );
}

#[test]
fn a_class_union_outside_weapons_reports_a_member_only_some_arms_have() {
    let mut ws = workspace();
    let id = ws.def_file(
        "lua/autorun/server/owner_union.lua",
        "---@return Player|NPC
local function pick() end
local who = pick()
who:LagCompensation(true)
local pos = pick():GetShootPos()",
    );
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
}

#[test]
fn a_helper_an_uncontracted_hook_also_calls_keeps_the_declared_owner() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think() self:ClearEffects() end
function SWEP:OnRemove() self:ClearEffects() end
function SWEP:ClearEffects() local owner = self:GetOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Entity", "NULL"]);
}

#[test]
fn a_helper_a_deferred_closure_also_calls_keeps_the_declared_owner() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think()
    self:ClearEffects()
    timer.Simple(1, function() self:ClearEffects() end)
end
function SWEP:ClearEffects() local owner = self:GetOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Entity", "NULL"]);
}

#[test]
fn a_helper_called_from_outside_the_class_keeps_the_declared_owner() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think() self:ClearEffects() end
function SWEP:ClearEffects() local owner = self:GetOwner() end",
    );
    ws.def_file(
        "gamemodes/testgm/gamemode/sv_drop.lua",
        "---@param weapon Weapon
function DropAll(weapon) weapon:ClearEffects() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Entity", "NULL"]);
}

#[test]
fn an_inherited_body_a_derived_npc_entry_calls_keeps_the_declared_owner() {
    let mut ws = workspace();
    let base = weapon(
        &mut ws,
        "weapon_owner_base/shared.lua",
        "function SWEP:Think() self:UseOwner() end
function SWEP:UseOwner() local owner = self:GetOwner() end",
    );
    weapon(
        &mut ws,
        "weapon_owner_derived/init.lua",
        "SWEP.Base = \"weapon_owner_base\"
function SWEP:NPCShoot_Primary() self:UseOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, base, 0)), ["Entity", "NULL"]);
}

#[test]
fn an_npc_entry_reaches_a_client_read_only_in_the_server_realm() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think() self:SharedHelper() end
function SWEP:NPCShoot_Primary() self:SharedHelper() end
function SWEP:SharedHelper()
    if CLIENT then local client_owner = self:GetOwner() end
    if SERVER then local server_owner = self:GetOwner() end
end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Player"]);
    assert_eq!(arms(&owner_type(&ws, id, 1)), ["NPC", "NULL", "Player"]);
}

#[test]
fn a_class_firing_npcs_separately_keeps_primary_attack_to_players() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "function SWEP:NPCShoot_Primary() end
function SWEP:PrimaryAttack() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, id, 0), "Player");
}

#[test]
fn a_class_implementing_the_getter_takes_no_contract() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "---@return Entity
function SWEP:GetOwner() return self end
function SWEP:Think() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, id, 0), "Entity");
}

#[test]
fn a_reassigned_self_takes_no_contract() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "---@param other Weapon
function SWEP:Think(other)
    local owner = self:GetOwner()
    self = other
end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Entity", "NULL"]);
}

#[test]
fn a_retained_npc_secondary_utility_reaches_secondary_attack() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "function SWEP:NPCShoot_Secondary() self:SecondaryAttack() end
function SWEP:SecondaryAttack() self:GetOwner():ChatPrint(\"ready\") end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "Player"]);
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
}

#[test]
fn a_reassigned_caller_keeps_its_helpers_owner_unknown() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "---@param other weapon_scope
function SWEP:Think(other) self = other self:UseOwner() end
function SWEP:UseOwner()
    local owner = self:GetOwner()
    if IsValid(owner) then owner:ChatPrint(\"ready\") end
end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Entity", "NULL"]);
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
}

#[test]
fn a_named_function_getter_assignment_takes_no_contract() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "---@return Entity
local function owner_override() return nil end
SWEP.GetOwner = owner_override
function SWEP:Think() self:GetOwner():ChatPrint(\"ready\") end",
    );
    assert_eq!(owner_type(&ws, id, 0), "Entity");
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
}

#[test]
fn a_direct_closure_npc_override_replaces_default_dispatch() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "SWEP.NPCShoot_Primary = function(self, pos, dir) end
function SWEP:PrimaryAttack() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, id, 0), "Player");
}

#[test]
fn a_direct_closure_npc_override_propagates_its_actual_calls() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "SWEP.NPCShoot_Primary = function(self, pos, dir) self:UseOwner() end
function SWEP:UseOwner() local owner = self:GetOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "NULL"]);
}

#[test]
fn an_alias_npc_override_replaces_default_dispatch_without_adding_edges() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "local function npc_override(self, pos, dir) self:UseOwner() end
local alias = npc_override
SWEP.NPCShoot_Primary = alias
function SWEP:PrimaryAttack() local owner = self:GetOwner() end
function SWEP:UseOwner() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, id, 0), "Player");
    assert_eq!(arms(&owner_type(&ws, id, 1)), ["Entity", "NULL"]);
}

#[test]
fn a_validity_only_override_inherits_its_class_contract() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "---@[self_call_valid(\"GetOwner\")]
function SWEP:Think() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, id, 0), "Player");
}

#[test]
fn an_explicit_class_contract_replaces_the_inherited_family() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/init.lua",
        "---@[self_call_type(\"GetOwner\", \"NPC\", \"server\")]
function SWEP:Think() local owner = self:GetOwner() end",
    );
    assert_eq!(owner_type(&ws, id, 0), "NPC");
}

#[test]
fn unrelated_attributes_preserve_inherited_entry_families() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "---@[deprecated]
function SWEP:NPCShoot_Secondary() self:SecondaryAttack() end
---@[deprecated]
function SWEP:SecondaryAttack() local owner = self:GetOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "Player"]);
}

#[test]
fn an_unrelated_entity_method_does_not_add_a_weapon_entry() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think() self:Drop() end
function SWEP:Drop() local owner = self:GetOwner() end",
    );
    ws.def_file(
        "gamemodes/testgm/entities/entities/hat/shared.lua",
        "function ENT:Drop() end",
    );
    ws.def_file(
        "gamemodes/testgm/gamemode/sv_hat.lua",
        "---@class HatCarrier : Player
---@field hat hat
local HatCarrier = {}
---@param ply HatCarrier
function DropHat(ply) ply.hat:Drop() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Player"]);
}

#[test]
fn self_presence_tests_do_not_add_entries() {
    for check in [
        "if self.PlayerEffects then self:PlayerEffects() end",
        "local checked = self.PlayerEffects and self:PlayerEffects()",
        "if not self.PlayerEffects then return end self:PlayerEffects()",
        "if self.PlayerEffects ~= nil then self:PlayerEffects() end",
    ] {
        let mut ws = workspace();
        let id = weapon(
            &mut ws,
            "weapon_scope/shared.lua",
            &format!(
                "function SWEP:Think() {check} end
function SWEP:PlayerEffects() local owner = self:GetOwner() end"
            ),
        );
        assert_eq!(arms(&owner_type(&ws, id, 0)), ["Player"], "{check}");
    }
}

#[test]
fn an_escaping_self_member_read_keeps_the_owner_unknown() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think() self:UseOwner() timer.Simple(0, self.UseOwner) end
function SWEP:UseOwner() local owner = self:GetOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Entity", "NULL"]);
}

#[test]
fn an_omitted_contract_realm_uses_the_client_hook_realm() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:DrawHUD() self:UseOwner() end
function SWEP:UseOwner()
    if CLIENT then local owner = self:GetOwner() end
    if SERVER then local owner = self:GetOwner() end
end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NULL", "Player"]);
    assert_eq!(arms(&owner_type(&ws, id, 1)), ["Entity", "NULL"]);
}

#[test]
fn a_receiver_query_that_reenters_its_own_body_stays_unknown() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think() self:UseOwner() end
function SWEP:UseOwner() self:GetOwner():UseOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Entity", "NULL"]);
}

#[test]
fn definitions_on_meta_declaring_classes_do_not_replace_native_contracts() {
    let mut ws = workspace();
    ws.def_file(
        "lua/annotations/native_definitions.lua",
        "---@return Entity|NULL
function Entity:GetOwner() end
function Weapon:NPCShoot_Primary() end",
    );
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        "function SWEP:PrimaryAttack() local owner = self:GetOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "Player"]);
}

#[test]
fn an_omitted_contract_realm_uses_the_server_hook_realm() {
    let mut ws = workspace();
    ws.def_file(
        "lua/annotations/server_entry.lua",
        "---@meta

---@class Weapon
local Weapon = {}

---@hook ServerEntry
---@realm server
---@[self_call_type(\"GetOwner\", \"NPC\")]
function Weapon:ServerEntry() end",
    );
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:ServerEntry() self:UseOwner() end
function SWEP:UseOwner()
    if SERVER then local owner = self:GetOwner() end
    if CLIENT then local owner = self:GetOwner() end
end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "NULL"]);
    assert_eq!(arms(&owner_type(&ws, id, 1)), ["Entity", "NULL"]);
}

#[test]
fn annotation_only_primary_dry_fire_keeps_reload_npc_reachable() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        r#"SWEP.Base = "weapon_base"
function SWEP:PrimaryAttack() if not self:CanPrimaryAttack() then return end end
function SWEP:Reload() self:GetOwner():GetAmmoCount("Pistol") end"#,
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "Player"]);
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
}

#[test]
fn overriding_can_primary_attack_removes_its_default_reload_edge() {
    for implementation in [
        "function SWEP:CanPrimaryAttack() return true end",
        "SWEP.CanPrimaryAttack = function(self) return true end",
        "local function can_attack(self) return true end SWEP.CanPrimaryAttack = can_attack",
    ] {
        let mut ws = workspace();
        let id = weapon(
            &mut ws,
            "weapon_gun/init.lua",
            &format!(
                r#"SWEP.Base = "weapon_base"
function SWEP:PrimaryAttack() if not self:CanPrimaryAttack() then return end end
{implementation}
function SWEP:Reload() self:GetOwner():GetAmmoCount("Pistol") end"#
            ),
        );
        assert_eq!(owner_type(&ws, id, 0), "Player", "{implementation}");
        assert_eq!(
            diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
            0
        );
    }
}

#[test]
fn annotation_only_stock_dispatch_reaches_overridden_helpers() {
    for (method, expected) in [
        ("CanSecondaryAttack", vec!["NPC", "Player"]),
        ("ShootBullet", vec!["NPC", "Player"]),
        ("ShootEffects", vec!["NPC", "Player"]),
        ("TakePrimaryAmmo", vec!["NPC", "Player"]),
        ("TakeSecondaryAmmo", vec!["NPC", "Player"]),
        ("Ammo1", vec!["NPC", "Player"]),
        ("Ammo2", vec!["NPC", "Player"]),
    ] {
        let mut ws = workspace();
        let id = weapon(
            &mut ws,
            "weapon_gun/init.lua",
            &format!("function SWEP:{method}() local owner = self:GetOwner() end"),
        );
        assert_eq!(arms(&owner_type(&ws, id, 0)), expected, "{method}");
    }
}

#[test]
fn annotation_only_npc_secondary_dispatch_propagates_explicit_callers() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:OnRemove() self:NPCShoot_Secondary() end
function SWEP:SecondaryAttack() local owner = self:GetOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Entity", "NULL"]);
}

fn guarded_chat_receiver_type(ws: &VirtualWorkspace, id: FileId) -> String {
    let model = ws.analysis.compilation.get_semantic_model(id).unwrap();
    let index = model
        .get_root()
        .descendants::<LuaCallExpr>()
        .find_map(|call| match call.get_prefix_expr()? {
            LuaExpr::IndexExpr(index) if index.syntax().text() == "owner:ChatPrint" => Some(index),
            _ => None,
        })
        .unwrap();
    render_contract_type(
        ws,
        model.infer_expr(index.get_prefix_expr().unwrap()).unwrap(),
    )
}

fn render_contract_type(ws: &VirtualWorkspace, typ: LuaType) -> String {
    match typ {
        LuaType::Union(union) => {
            let arms = union
                .into_vec()
                .into_iter()
                .map(|arm| ws.humanize_type(arm))
                .collect::<Vec<_>>();
            format!("({})", arms.join("|"))
        }
        typ => ws.humanize_type(typ),
    }
}

#[test]
fn a_different_getters_contract_does_not_describe_an_owner_entry() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        r#"function SWEP:Think() self:UseOwner() end
---@[self_call_type("GetParent", "Entity")]
function SWEP:OnRemove() self:UseOwner() end
function SWEP:UseOwner()
    local owner = self:GetOwner()
    if IsValid(owner) then owner:ChatPrint("removed") end
end"#,
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Entity", "NULL"]);
    assert_eq!(guarded_chat_receiver_type(&ws, id), "Player");
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
    let reports = ws
        .analysis
        .diagnose_file(id, CancellationToken::new())
        .unwrap();
    assert!(reports[0].message.contains("Entity"));
    assert!(reports[0].message.contains("Player"));
}

#[test]
fn one_root_can_describe_both_getters_without_losing_owner_inference() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        r#"---@[self_call_type("GetParent", "NPC")]
---@[self_call_type("GetOwner", "Player")]
function SWEP:OnRemove() self:UseOwner() end
function SWEP:UseOwner()
    local parent = self:GetParent()
    local owner = self:GetOwner()
    if IsValid(owner) then owner:ChatPrint("removed") end
end"#,
    );
    let model = ws.analysis.compilation.get_semantic_model(id).unwrap();
    let parent = model
        .get_root()
        .descendants::<LuaCallExpr>()
        .find(|call| call.syntax().text() == "self:GetParent()")
        .unwrap();
    let parent = model.infer_expr(LuaExpr::CallExpr(parent)).unwrap();
    assert_eq!(arms(&render_contract_type(&ws, parent)), ["NPC", "NULL"]);
    let owner = model
        .get_root()
        .descendants::<LuaCallExpr>()
        .find(|call| call.syntax().text() == "self:GetOwner()")
        .unwrap();
    let owner = model.infer_expr(LuaExpr::CallExpr(owner)).unwrap();
    assert_eq!(arms(&render_contract_type(&ws, owner)), ["NULL", "Player"]);
    assert_eq!(guarded_chat_receiver_type(&ws, id), "Player");
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        0
    );
}

#[test]
fn a_validity_guard_removes_null_without_selecting_player() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        r#"function SWEP:NPCShoot_Primary() self:UseOwner() end
function SWEP:PrimaryAttack() self:UseOwner() end
function SWEP:UseOwner()
    local owner = self:GetOwner()
    if IsValid(owner) then owner:ChatPrint("ready") end
end"#,
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "NULL", "Player"]);
    assert_eq!(
        arms(&guarded_chat_receiver_type(&ws, id)),
        ["NPC", "Player"]
    );
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
}

#[test]
fn annotation_only_secondary_dispatch_keeps_both_branch_warnings() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "ix_hands/init.lua",
        r#"function SWEP:SecondaryAttack(door)
    if door then
        self:GetOwner():ViewPunch(nil)
    else
        self:GetOwner():SetLocalVar("bIsHoldingObject", true)
    end
end"#,
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "Player"]);
    assert_eq!(arms(&owner_type(&ws, id, 1)), ["NPC", "Player"]);
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        2
    );
}

#[test]
fn replacing_npc_secondary_dispatch_removes_only_its_npc_route() {
    for implementation in [
        "function SWEP:NPCShoot_Secondary() end",
        "SWEP.NPCShoot_Secondary = function(self) end",
        "local function no_dispatch(self) end SWEP.NPCShoot_Secondary = no_dispatch",
    ] {
        let mut ws = workspace();
        let id = weapon(
            &mut ws,
            "weapon_gun/init.lua",
            &format!(
                r#"{implementation}
function SWEP:SecondaryAttack() self:GetOwner():ChatPrint("ready") end
function SWEP:PrimaryAttack() self:GetOwner():ChatPrint("ready") end"#
            ),
        );
        assert_eq!(owner_type(&ws, id, 0), "Player", "{implementation}");
        assert_eq!(
            arms(&owner_type(&ws, id, 1)),
            ["NPC", "Player"],
            "{implementation}"
        );
        assert_eq!(
            diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
            1
        );
    }
}

#[test]
fn the_retained_npc_secondary_contract_applies_only_on_the_server() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/shared.lua",
        "function SWEP:SecondaryAttack()
    if SERVER then self:GetOwner():ChatPrint(\"server\") end
    if CLIENT then self:GetOwner():ChatPrint(\"client\") end
end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "Player"]);
    assert_eq!(owner_type(&ws, id, 1), "Player");
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
}

#[test]
fn rejecting_npc_owners_in_secondary_attack_keeps_player_calls_valid() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        r#"function SWEP:SecondaryAttack()
    local owner = self:GetOwner()
    if owner:IsNPC() then return end
    owner:ChatPrint("ready")
end"#,
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "Player"]);
    assert_eq!(guarded_chat_receiver_type(&ws, id), "Player");
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        0
    );
}

#[test]
fn an_inherited_secondary_dispatch_override_keeps_player_only_secondary_paths() {
    let mut ws = workspace();
    weapon(
        &mut ws,
        "weapon_player_base/shared.lua",
        "function SWEP:NPCShoot_Secondary() end",
    );
    let id = weapon(
        &mut ws,
        "weapon_gun/init.lua",
        r#"SWEP.Base = "weapon_player_base"
function SWEP:SecondaryAttack() self:UseOwner() end
function SWEP:UseOwner() self:GetOwner():ChatPrint("ready") end
function SWEP:PrimaryAttack() self:GetOwner():ChatPrint("ready") end"#,
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Player"]);
    assert_eq!(arms(&owner_type(&ws, id, 1)), ["NPC", "Player"]);
    assert_eq!(
        diagnostics(&mut ws, id, DiagnosticCode::InferUnguardedChild),
        1
    );
}

#[test]
fn reload_helper_keeps_entry_validity_for_a_redundant_owner_truthiness_check() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_reload/shared.lua",
        r#"function SWEP:Think() self:PerformReload() end
function SWEP:PerformReload()
    local ply = self:GetOwner()
    if not ply or ply:GetAmmoCount("Buckshot") <= 0 then return end
end"#,
    );
    assert_eq!(owner_type(&ws, id, 0), "Player");
    assert_eq!(diagnostics(&mut ws, id, DiagnosticCode::GmodNullCheck), 0);
    assert_eq!(diagnostics(&mut ws, id, DiagnosticCode::NeedCheckNil), 0);
}

#[test]
fn reload_helper_reached_from_an_uncontracted_hook_keeps_null_truthiness_warning() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_reload/shared.lua",
        r#"function SWEP:Think() self:PerformReload() end
function SWEP:OnRemove() self:PerformReload() end
function SWEP:PerformReload()
    local ply = self:GetOwner()
    if not ply then return end
end"#,
    );
    assert_eq!(diagnostics(&mut ws, id, DiagnosticCode::GmodNullCheck), 1);
}

#[test]
fn a_helper_inherits_validity_through_an_intermediate_hook() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/init.lua",
        "function SWEP:PrimaryAttack() self:FirstHelper() end
function SWEP:FirstHelper() self:UseOwner() end
function SWEP:UseOwner() local owner = self:GetOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "Player"]);
}

#[test]
fn one_root_reaching_a_helper_with_and_without_validity_keeps_null() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/init.lua",
        "function SWEP:NPCShoot_Primary() self:PrimaryAttack() self:UseOwner() end
function SWEP:PrimaryAttack() self:UseOwner() end
function SWEP:UseOwner() self:Cycle() local owner = self:GetOwner() end
function SWEP:Cycle() self:UseOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NPC", "NULL", "Player"]);
}

#[test]
fn a_validity_contract_for_another_getter_keeps_owner_null() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        r#"---@[self_call_type("GetOwner", "Player")]
---@[self_call_valid("GetParent")]
function SWEP:OnRemove() self:UseOwner() end
function SWEP:UseOwner() local owner = self:GetOwner() end"#,
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["NULL", "Player"]);
}

#[test]
fn a_deferred_entry_keeps_null_despite_a_valid_hook_entry() {
    let mut ws = workspace();
    let id = weapon(
        &mut ws,
        "weapon_scope/shared.lua",
        "function SWEP:Think()
    self:UseOwner()
    timer.Simple(0, function() self:UseOwner() end)
end
function SWEP:UseOwner() local owner = self:GetOwner() end",
    );
    assert_eq!(arms(&owner_type(&ws, id, 0)), ["Entity", "NULL"]);
}
