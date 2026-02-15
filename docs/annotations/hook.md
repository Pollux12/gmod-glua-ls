# @hook - Hook Registration

Marks a method as a hook handler, enabling hook name autocomplete and analysis.

## Syntax

```lua
---@hook [hook_name]
```

- Without `hook_name`: uses the method name as the hook name
- With `hook_name`: registers the specified hook name

## Examples

### Gamemode Hooks

```lua
-- Automatically treated as hook (no @hook needed for GM/GAMEMODE)
function GM:PlayerSpawn(ply)
    -- Called when a player spawns
end

-- Explicit @hook with custom name
---@hook PlayerInitialSpawn
function GM:OnPlayerFirstJoin(ply)
    -- Registered as "PlayerInitialSpawn" hook
end
```

### Plugin/Addon Hooks

```lua
local PLUGIN = {}

-- Mark PLUGIN methods as hooks
---@hook
function PLUGIN:PlayerSpawn(ply)
    -- Registered as "PlayerSpawn" hook
end

---@hook CustomEvent
function PLUGIN:HandleCustomEvent(data)
    -- Registered as "CustomEvent" hook
end

-- The hook is now available in autocomplete
-- hook.Run("CustomEvent", { ... })
-- hook.Add("CustomEvent", ...)
```

### Custom Frameworks

```lua
MYFRAMEWORK = {}

---@hook
function MYFRAMEWORK:PlayerLoaded(ply)
    -- Works with hookMappings.methodPrefixes config
end
```

## Usage with hook.Add/hook.Run

Once registered via `---@hook`, the hook name appears in autocomplete:

```lua
-- These will suggest "PlayerSpawn" and "CustomEvent"
hook.Run("|")           -- | = cursor position
hook.Call("|", ...)
hook.Add("|", ...)
```

## See Also

- [Configuration](../../config.md) — `hookMappings` for custom prefixes and mappings
