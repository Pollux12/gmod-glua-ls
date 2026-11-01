---@meta
---@class Entity
local Entity = {}
---@class NULL
---@class Player : Entity
local Player = {}
---@class Weapon : Entity
local Weapon = {}
---@class SWEP : Weapon
---Returns the owner entity of this entity.
---@realm shared
---@source https://wiki.facepunch.com/gmod/Entity:GetOwner
---@return Entity|NULL # The owner entity of this entity, or NULL when it has no owner.
function Entity:GetOwner() end

---Checks if the entity is a player or not.
---@realm shared
---@source https://wiki.facepunch.com/gmod/Entity:IsPlayer
---@return boolean # Whether the entity is a player.
---@return_cast self Player
function Entity:IsPlayer() end

---Gets the position of given entity in the world.
---
--- See [Entity:GetLocalPos](https://wiki.facepunch.com/gmod/Entity:GetLocalPos) for the position relative to the entity's [Entity:GetParent](https://wiki.facepunch.com/gmod/Entity:GetParent).
---@realm shared
---@source https://wiki.facepunch.com/gmod/Entity:GetPos
---@return Vector # The position of the entity.
function Entity:GetPos() end

---Set a player's FOV (Field Of View) over a certain amount of time.
---
--- This is meant to be called on the server or shared (for prediction), it will have no effect if called clientside only. You may want to use [GM:CalcView](https://wiki.facepunch.com/gmod/GM:CalcView) for that instead.
---@realm shared
---@source https://wiki.facepunch.com/gmod/Player:SetFOV
---@param fov number the angle of perception (FOV). Set to 0 to return to default user FOV. ( Which is ranging from 75 to 100, depending on user settings )
---@param time? number the time it takes to transition to the FOV expressed in a floating point.
---@param requester? Entity The requester or "owner" of the zoom event. Only this entity will be able to change the player's FOV until it is set back to 0.
function Player:SetFOV(fov, time, requester) end

---Returns the player's active weapon.
---
--- If used on a [Global.LocalPlayer](https://wiki.facepunch.com/gmod/Global.LocalPlayer)() and the player is spectating another player with `OBS_MODE_IN_EYE`, the weapon returned will be of the spectated player.
---@realm shared
---@source https://wiki.facepunch.com/gmod/Player:GetActiveWeapon
---@return Weapon # The weapon the player currently has equipped or NULL if the player doesn't have an active weapon eg. when they're dead.
function Player:GetActiveWeapon() end

---@[valid_guard]
---@param value any
---@return TypeGuard<Entity>
function IsValid(value) end
