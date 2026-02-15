# @cast - Type Casting

Force type conversion for variables, used for type narrowing or extension.

## Syntax

```lua
---@cast <variable_name> [+|-]<type>[, [+|-]<type>...]
```

## Examples

```lua
---@param value any
function processValue(value)
    if type(value) == "string" then
        ---@cast value string
        print("String length:", #value)  -- value is now confirmed as string type
    end

    if type(value) == "table" and value.id then
        ---@cast value {id: number, name?: string}
        print("ID:", value.id)           -- value now has id field
    end
end

-- Add type to union type
---@type string | number
local mixedValue = getValue()

if needsBoolean() then
    ---@cast mixedValue +boolean  -- Add boolean type
    mixedValue = true
end

-- Remove type from union type
---@type string | number | nil
local maybeValue = getMaybeValue()

if maybeValue then
    ---@cast maybeValue -nil      -- Remove nil type
    print("Value:", maybeValue)   -- maybeValue is now string | number
end

-- Complex type casting
---@type table
local data = parseJSON(jsonString)

-- Cast to specific structure
---@cast data {users: {id: number, name: string}[]}
for _, user in ipairs(data.users) do
    print("User:", user.name, "ID:", user.id)
end

-- Type narrowing example
---@param input string | number | boolean
function handleInput(input)
    if type(input) == "string" and input:match("^%d+$") then
        ---@cast input string  -- Confirm it's string type
        local num = tonumber(input)
        ---@cast num number    -- tonumber result confirmed as number
        print("Number:", num)
    end
end

-- Add multiple types
---@type string
local value = "initial"

---@cast value +number, +boolean  -- Add number and boolean types
-- value is now string | number | boolean

-- Remove multiple types
---@type string | number | boolean | nil
local multiValue = getMultiValue()

---@cast multiValue -boolean, -nil  -- Remove boolean and nil types
-- multiValue is now string | number
```

## Features

1. **Type narrowing**
2. **Type extension**
3. **Union type operations**
4. **Multiple type operations simultaneously**
5. **Runtime type confirmation**
