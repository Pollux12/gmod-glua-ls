# Annotations Reference

Type annotations for GLua development using EmmyLua/LuaCATS syntax.

## Type System

| Annotation | Description | Example |
|------------|-------------|---------|
| [`@class`](./class.md) | Define a class | `---@class Entity` |
| [`@field`](./field.md) | Add field to class | `---@field health number` |
| [`@type`](./type.md) | Declare variable type | `---@type Player` |
| [`@alias`](./alias.md) | Type alias | `---@alias ID string \| number` |
| [`@enum`](./enum.md) | Enumeration | `---@enum TEAM` |
| [`@generic`](./generic.md) | Generic types | `---@generic T` |

## Functions

| Annotation | Description | Example |
|------------|-------------|---------|
| [`@param`](./param.md) | Parameter type | `---@param ply Player` |
| [`@return`](./return.md) | Return type | `---@return boolean` |
| [`@overload`](./overload.md) | Multiple signatures | `---@overload fun(x: number)` |
| [`@async`](./async.md) | Async marker | `---@async` |
| [`@nodiscard`](./nodiscard.md) | Must use return | `---@nodiscard` |

## GMod-Specific

| Annotation | Description | Example |
|------------|-------------|---------|
| [`@realm`](./realm.md) | Declare realm | `---@realm server` |
| [`@hook`](./hook.md) | Register hook | `---@hook PlayerSpawn` |

## Other

| Annotation | Description | Example |
|------------|-------------|---------|
| [`@deprecated`](./deprecated.md) | Mark deprecated | `---@deprecated Use NewFunc()` |
| [`@diagnostic`](./diagnostic.md) | Control warnings | `---@diagnostic disable-next-line` |
| [`@cast`](./cast.md) | Type cast | `---@cast ply Player` |
| [`@meta`](./meta.md) | Meta file marker | `---@meta` |
| [`@module`](./module.md) | Module declaration | `---@module "mylib"` |
| [`@see`](./see.md) | Cross-reference | `---@see AnotherFunction` |

## Quick Examples

```lua
---@class Player
---@field SteamID64 fun(self: Player): string
---@field IsAdmin fun(self: Player): boolean

---@realm server
---@param target Player
---@param amount number
---@return boolean success
function GiveMoney(target, amount)
    if not target:IsAdmin() then return false end
    -- ...
    return true
end

---@hook
function PLUGIN:PlayerSpawn(ply)
    -- Registered as hook
end
```
