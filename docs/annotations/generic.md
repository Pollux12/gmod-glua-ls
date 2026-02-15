# @generic - Generic Definition

Define generic parameters to achieve code reuse and type safety.

## Syntax

```lua
---@generic <generic_name1>[: <constraint_type1>] [, <generic_name2>[: <constraint_type2>]...]
```

## Examples

```lua
-- Basic generic function
---@generic T
---@param value T Input value
---@return T Output value of same type
function identity(value)
    return value
end

-- Usage examples
local str = identity("hello")      -- str type is string
local num = identity(42)           -- num type is number

-- Multiple generic parameters
---@generic K, V
---@param map table<K, V> Map table
---@return K[] Array of all keys
function getKeys(map)
    local keys = {}
    for k in pairs(map) do
        table.insert(keys, k)
    end
    return keys
end

---@generic K, V
---@param map table<K, V> Map table
---@return V[] Array of all values
function getValues(map)
    local values = {}
    for _, v in pairs(map) do
        table.insert(values, v)
    end
    return values
end

-- Generic constraints
---@generic T : table
---@param obj T Object that must be a table
---@return T Cloned object
function deepClone(obj)
    local clone = {}
    for k, v in pairs(obj) do
        if type(v) == "table" then
            clone[k] = deepClone(v)
        else
            clone[k] = v
        end
    end
    return clone
end

-- Generic class
---@generic T
---@class Stack<T>
---@field private items T[]
local Stack = {}

---@param self Stack<T>
---@param item T
function Stack:push(item)
    table.insert(self.items, item)
end

---@param self Stack<T>
---@return T?
function Stack:pop()
    return table.remove(self.items)
end

---@generic T
---@return Stack<T>
function Stack.new()
    return setmetatable({items = {}}, {__index = Stack})
end

-- Usage examples
local stringStack = Stack.new()  -- Stack<string>
stringStack:push("hello")
stringStack:push("world")

local numberStack = Stack.new()  -- Stack<number>
numberStack:push(1)
numberStack:push(2)

-- Generic array operations
---@generic T
---@param array T[] Array to filter
---@param predicate fun(item: T): boolean Filter predicate
---@return T[] Filtered array
function filter(array, predicate)
    local result = {}
    for _, item in ipairs(array) do
        if predicate(item) then
            table.insert(result, item)
        end
    end
    return result
end

---@generic T, U
---@param array T[] Array to map
---@param mapper fun(item: T): U Mapping function
---@return U[] Mapped array
function map(array, mapper)
    local result = {}
    for _, item in ipairs(array) do
        table.insert(result, mapper(item))
    end
    return result
end

-- Usage examples
local numbers = {1, 2, 3, 4, 5}
local evenNumbers = filter(numbers, function(n) return n % 2 == 0 end)
local doubled = map(numbers, function(n) return n * 2 end)

local names = {"John", "Jane", "Bob"}
local lengths = map(names, function(name) return #name end)
```

## Features

1. **Type parameter definition**
2. **Generic constraints**
3. **Generic classes**
4. **Multiple generic parameters**
5. **Type inference**
