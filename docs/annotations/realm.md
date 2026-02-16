# @realm - Realm Declaration

Explicitly declares which realm (client, server, or shared) code belongs to. Used for realm validation and inline hints.

## Syntax

```lua
---@realm client|server|shared
```

## File-Level Realm

Note: This is optional, by default we detect file realm by checking file prefix, and if that is inconclusive, infer based on loading and function usage.

Place at the top of a file to declare the entire file's realm:

```lua
---@realm client

-- This entire file is marked as client-side only
function MyHUD()
    -- Drawing HUD elements
end
```

## Function-Level Realm

Note: This is optional, by default we assign the realm based on the file realm, but also account for `if SERVER` or `if CLIENT` blocks within shared files.

Use above a function to override the file's default realm:

```lua
---@realm server
function SpawnEntity(entClass, pos)
    -- This function is marked as server-side
    return ents.Create(entClass)
end
```

## Realm Detection

The language server automatically detects realms from:

1. **Filename prefixes**: `cl_` (client), `sv_` (server), `sh_` (shared)
2. **Folder names**: `client/`, `server/`, `shared/`
3. **API calls**: Using `AddCSLuaFile()`, `net` functions, etc.
4. **@realm annotation**: Explicit declaration (highest priority)

## Realm Mismatch Warnings

When realm is known, the server warns about mismatched API usage:

```lua
---@realm client

-- Warning: ents.Create is a server-side function
local ent = ents.Create("prop_physics")  -- ERROR: client/server mismatch
```

## Examples

```lua
-- Shared file with server-only function
---@realm shared

---@realm server
function CreateProp(pos)
    return ents.Create("prop_physics")
end

---@realm client
function DrawOverlay()
    draw.SimpleText("Hello", ...)
end

-- Shared function (no @realm = inherits file default)
function SharedLogic()
    -- Available on both realms
end
```

## See Also

- [Configuration](../config.md) — `gmod.detectRealmFromFilename` and `gmod.detectRealmFromCalls`
