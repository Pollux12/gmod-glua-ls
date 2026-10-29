---@meta
---@attribute self_call_valid(method: string)
---@attribute self_call_type(method: string, class: string, realm: "client"|"server"|nil)
---@attribute default_self_call(method: string)
---@attribute not_engine_called()

---@class Entity
local Entity = {}
---@class NULL : Entity
local NULL = {}
---@class Player : Entity
local Player = {}
---@class NPC : Entity
local NPC = {}
---@class Weapon : Entity
local Weapon = {}
---@class WEAPON : Weapon
---@class SWEP : WEAPON

---@return Entity|NULL
function Entity:GetOwner() end

---@return Entity|NULL
function Entity:GetParent() end

---@return boolean
---@return_cast self NPC
function Entity:IsNPC() end

---@return Vector
function Player:GetShootPos() end

---@realm server
---@return Vector
function NPC:GetShootPos() end

---@param draw boolean
function Player:DrawViewModel(draw) end

---@param enable boolean
function Player:LagCompensation(enable) end

---@param ammoType string
---@return number
function Player:GetAmmoCount(ammoType) end

---@hook Think
---@[self_call_valid("GetOwner")]
---@[self_call_type("GetOwner", "Player")]
function Weapon:Think() end

---@hook Deploy
---@return boolean?
---@[self_call_valid("GetOwner")]
---@[self_call_type("GetOwner", "Player")]
function Weapon:Deploy() end

---@hook Reload
---@[self_call_valid("GetOwner")]
---@[self_call_type("GetOwner", "Player")]
function Weapon:Reload() end

---@hook PrimaryAttack
---@[self_call_valid("GetOwner")]
---@[self_call_type("GetOwner", "Player")]
---@[default_self_call("CanPrimaryAttack")]
---@[default_self_call("ShootBullet")]
---@[default_self_call("TakePrimaryAmmo")]
function Weapon:PrimaryAttack() end

---@hook NPCShoot_Primary
---@realm server
---@[self_call_type("GetOwner", "NPC", "server")]
---@[default_self_call("PrimaryAttack")]
function Weapon:NPCShoot_Primary() end

---@hook NPCShoot_Secondary
---@realm server
---@[not_engine_called]
---@[self_call_type("GetOwner", "NPC", "server")]
---@[default_self_call("SecondaryAttack")]
function Weapon:NPCShoot_Secondary() end

---@hook OnRemove
function Weapon:OnRemove() end

---@param owner Entity
function Entity:SetOwner(owner) end

---@param text string
function Player:ChatPrint(text) end

---@hook SecondaryAttack
---@[self_call_valid("GetOwner")]
---@[self_call_type("GetOwner", "Player")]
---@[default_self_call("CanSecondaryAttack")]
---@[default_self_call("ShootBullet")]
---@[default_self_call("TakeSecondaryAmmo")]
function Weapon:SecondaryAttack() end

---@hook Holster
---@param weapon? Weapon
---@return boolean
function Weapon:Holster(weapon) end

---@param value any
---@return boolean
---@[valid_guard]
function IsValid(value) end

timer = {}
---@param delay number
---@param callback fun()
function timer.Simple(delay, callback) end

---@return boolean
---@[not_engine_called]
---@[default_self_call("Reload")]
function Weapon:CanPrimaryAttack() end

---@return boolean
---@[not_engine_called]
function Weapon:CanSecondaryAttack() end

---@hook DrawHUD
---@realm client
---@[self_call_type("GetOwner", "Player")]
function Weapon:DrawHUD() end

---@[not_engine_called]
---@[default_self_call("ShootEffects")]
function Weapon:ShootBullet() end

---@[not_engine_called]
function Weapon:ShootEffects() end

---@[not_engine_called]
---@[default_self_call("Ammo1")]
function Weapon:TakePrimaryAmmo() end

---@[not_engine_called]
---@[default_self_call("Ammo2")]
function Weapon:TakeSecondaryAmmo() end

---@[not_engine_called]
function Weapon:Ammo1() end

---@[not_engine_called]
function Weapon:Ammo2() end

---@param angle any
function Player:ViewPunch(angle) end

---@param name string
---@param value any
function Player:SetLocalVar(name, value) end
