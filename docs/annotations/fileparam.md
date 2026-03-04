# @fileparam - File-Level Parameter Hint

Declares a file-level parameter type hint. Any parameter with the specified name in the current file will default to this type if it lacks an explicit `@param` annotation.

## Syntax

```lua
---@fileparam <paramName> <type>
```

## Description

The `@fileparam` annotation allows you to establish a convention for parameter naming within a specific file, saving you from repeating `@param` annotations for common parameter names like `ply`, `ent`, or `vehicle`.

When a function defines a parameter that matches `<paramName>` but does not have a specific `@param` annotation for it, the language server will automatically assume it is of type `<type>`.

## Example

```lua
---@fileparam vehicle base_glide
---@fileparam ply Player

-- 'vehicle' is automatically typed as 'base_glide'
-- 'ply' is automatically typed as 'Player'
local function enter(vehicle, ply)
    local seat = vehicle:GetFreeSeat()
    ply:EnterVehicle(seat)
end

-- Explicit @param annotations still take precedence over @fileparam
---@param vehicle Entity
local function takeDamage(vehicle, dmginfo)
    -- 'vehicle' is typed as 'Entity' here due to explicit @param
end
```

## Precedence

When determining the type of an unannotated parameter, the language server checks in this order:

1. **Explicit annotations**: `---@param ply Player` above the function.
2. **File-level hints**: `---@fileparam ply Player` at the top of the file.
3. **Workspace defaults**: Configured in `.gluarc.json` under `gmod.fileParamDefaults`.

## See Also

- [Configuration](../config.md) — For project-wide `gmod.fileParamDefaults`
