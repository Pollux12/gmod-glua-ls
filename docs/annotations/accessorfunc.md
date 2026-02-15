# @accessorfunc - Accessor Generator

Marks a function as an accessor generator. Calls to that function synthesize `Get{Name}` and `Set{Name}` methods on the calling class.

## Syntax

```lua
---@accessorfunc
---@accessorfunc N
```

- `N` is a 1-indexed parameter position for the accessor name argument
- Without `N`, the first argument is used

## Examples

### Basic Usage

```lua
---@accessorfunc
function ENT:RegisterAccessor(name)
    -- custom accessor logic
end

function ENT:SetupDataTables()
    self:RegisterAccessor("Health")
end

-- Synthesized for ENT:
-- ENT:GetHealth(): any
-- ENT:SetHealth(value: any): nil
```

### Multiple Calls

```lua
---@accessorfunc
function ENT:RegisterAccessor(name)
end

function ENT:SetupDataTables()
    self:RegisterAccessor("Health")
    self:RegisterAccessor("Armor")
    self:RegisterAccessor("TeamName")
end

-- Synthesized: GetHealth/SetHealth, GetArmor/SetArmor, GetTeamName/SetTeamName
```

### Custom Name Parameter Index

```lua
---@accessorfunc 3
function ENT:RegisterTypedAccessor(var_type, slot, name)
    -- here, arg #3 is the accessor name
end

function ENT:SetupDataTables()
    self:RegisterTypedAccessor("Float", 0, "Speed")
end

-- Synthesized: ENT:GetSpeed(), ENT:SetSpeed(value)
```

## Notes

- Works on any class, not only GMod scripted classes
- Not gated by `gmod.enabled`
- Current limitation: synthesized getters return `any`, and setters accept `any`

## See Also

- [Configuration](../config.md#scripted-class-analysis) — Scripted class analysis and synthesis behavior
