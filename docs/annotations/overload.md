# @overload - Function Overload

Define multiple function signatures for the same function, supporting different parameter combinations.

## Syntax

```lua
---@overload fun(<parameters>): <return_types>
```

## Examples

```lua
-- Basic function overloads
---@overload fun(x: number): number
---@overload fun(x: number, y: number): number
---@overload fun(x: number, y: number, z: number): number
---@param x number
---@param y? number
---@param z? number
---@return number
function add(x, y, z)
    return x + (y or 0) + (z or 0)
end

-- String/number handling
---@overload fun(value: string): string
---@overload fun(value: number): string
---@overload fun(value: boolean): string
---@param value any
---@return string
function toString(value)
    if type(value) == "string" then
        return value
    elseif type(value) == "number" then
        return tostring(value)
    elseif type(value) == "boolean" then
        return value and "true" or "false"
    else
        return "unknown"
    end
end

-- Constructor overloads
---@class Vector
---@field x number
---@field y number
---@field z number
local Vector = {}

---@overload fun(): Vector
---@overload fun(x: number): Vector
---@overload fun(x: number, y: number): Vector
---@overload fun(x: number, y: number, z: number): Vector
---@param x? number
---@param y? number
---@param z? number
---@return Vector
function Vector.new(x, y, z)
    return setmetatable({
        x = x or 0,
        y = y or 0,
        z = z or 0
    }, {__index = Vector})
end

-- API endpoint overloads
---@overload fun(endpoint: string): table
---@overload fun(endpoint: string, options: {method?: string, headers?: table}): table
---@overload fun(endpoint: string, data: table): table
---@overload fun(endpoint: string, data: table, options: {method?: string, headers?: table}): table
---@param endpoint string
---@param dataOrOptions? table
---@param options? table
---@return table
function apiRequest(endpoint, dataOrOptions, options)
    local finalOptions = {}
    local data = nil

    if dataOrOptions then
        if options then
            -- apiRequest(endpoint, data, options)
            data = dataOrOptions
            finalOptions = options
        elseif dataOrOptions.method or dataOrOptions.headers then
            -- apiRequest(endpoint, options)
            finalOptions = dataOrOptions
        else
            -- apiRequest(endpoint, data)
            data = dataOrOptions
        end
    end

    -- Perform request with processed parameters
    return performRequest(endpoint, data, finalOptions)
end

-- Generic overloads
---@overload fun<T>(items: T[]): T[]
---@overload fun<T>(items: T[], predicate: fun(item: T): boolean): T[]
---@generic T
---@param items T[]
---@param predicate? fun(item: T): boolean
---@return T[]
function filter(items, predicate)
    if not predicate then
        return items  -- Return copy of original array
    end

    local result = {}
    for _, item in ipairs(items) do
        if predicate(item) then
            table.insert(result, item)
        end
    end
    return result
end

-- Usage examples
local result1 = add(5)          -- Uses overload fun(x: number): number
local result2 = add(5, 10)      -- Uses overload fun(x: number, y: number): number
local result3 = add(5, 10, 15)  -- Uses overload fun(x: number, y: number, z: number): number

local str1 = toString(42)       -- Uses overload fun(value: number): string
local str2 = toString("hello")  -- Uses overload fun(value: string): string
local str3 = toString(true)     -- Uses overload fun(value: boolean): string

local vec1 = Vector.new()       -- Uses overload fun(): Vector
local vec2 = Vector.new(1)      -- Uses overload fun(x: number): Vector
local vec3 = Vector.new(1, 2)   -- Uses overload fun(x: number, y: number): Vector
local vec4 = Vector.new(1, 2, 3) -- Uses overload fun(x: number, y: number, z: number): Vector

local response1 = apiRequest("/users")
local response2 = apiRequest("/users", {method = "POST"})
local response3 = apiRequest("/users", {name = "John"})
local response4 = apiRequest("/users", {name = "John"}, {method = "POST"})
```

## Features

1. **Multiple function signatures**
2. **Parameter variation support**
3. **Type-specific overloads**
4. **Generic function overloads**
5. **Constructor overloading**
