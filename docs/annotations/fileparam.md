# @fileparam - File-Level Parameter Hint

Declares a file-level parameter type hint. If a parameter name matches and has no explicit `@param`, this type is used.

## Syntax

```lua
---@fileparam <paramName> <type>
```

## Description

`@fileparam` sets a file-wide naming rule so you do not need to repeat `@param` for common names like `ply`, `ent`, or `vehicle`.

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

The GLua settings editor also exposes `gmod.fileParamDefaults` as an editable mapping table so projects can add, replace, or remove the built-in fallback names without changing source annotations. If you edit `.gluarc.json` directly, an empty string value removes a built-in fallback for that workspace.

## See Also

- [Configuration](../config.md) — For project-wide `gmod.fileParamDefaults`

