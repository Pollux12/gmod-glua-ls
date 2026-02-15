# @param - Parameter Definition

Define type and description information for function parameters.

## Syntax

```lua
---@param <parameter_name>[?] <type_expression> [description]
```

## Parameter Features

- `?` - Optional parameter marker
- `...` - Variadic parameter marker
- Supports union types
- Supports generic parameters

## Examples

```lua
-- Basic parameter definition
---@param name string Username
---@param age number Age
function createUser(name, age)
    return {name = name, age = age}
end

-- Optional parameters (using ? marker)
---@param name string Username
---@param age? number Age (optional, defaults to 18)
---@param email? string Email (optional)
function registerUser(name, age, email)
    age = age or 18
    return {
        name = name,
        age = age,
        email = email
    }
end

-- Union type parameters
---@param id string | number User ID (string or number)
---@param options table | nil Configuration options (can be nil)
function getUserById(id, options)
    -- Handle different ID types
    local normalizedId = tostring(id)
    -- Use configuration options
    options = options or {}
end

-- Variadic parameters
---@param format string Format string
---@param ... any Formatting arguments
function printf(format, ...)
    print(string.format(format, ...))
end

-- Function type parameters
---@param data table Data
---@param callback fun(result: any, error: string?): nil Callback function
---@param onProgress? fun(percent: number): nil Progress callback (optional)
function processDataAsync(data, callback, onProgress)
    -- Process data asynchronously
    if onProgress then
        onProgress(50)  -- Report progress
    end

    -- Call callback when processing is complete
    local success, result = pcall(function()
        return processData(data)
    end)

    if success then
        callback(result, nil)
    else
        callback(nil, result)  -- result is error message
    end
end

-- Complex object parameters
---@param request {method: string, url: string, headers?: table<string, string>, body?: string}
---@param options? {timeout?: number, retries?: number}
function httpRequest(request, options)
    options = options or {}
    local timeout = options.timeout or 30
    local retries = options.retries or 3

    -- Send HTTP request
end

-- Generic parameters
---@generic T
---@param items T[] List of items
---@param predicate fun(item: T): boolean Filter predicate
---@return T[] Filtered list
function filter(items, predicate)
    local result = {}
    for _, item in ipairs(items) do
        if predicate(item) then
            table.insert(result, item)
        end
    end
    return result
end

-- Method parameters (self parameter)
---@class Calculator
local Calculator = {}

---@param self Calculator
---@param x number First number
---@param y number Second number
---@return number Calculation result
function Calculator:add(x, y)
    return x + y
end

-- Or use colon syntax (self automatically inferred)
---@param x number First number
---@param y number Second number
---@return number Calculation result
function Calculator:multiply(x, y)
    return x * y
end

-- Usage examples
local user1 = createUser("John", 25)
local user2 = registerUser("Jane")  -- age uses default value
local user3 = registerUser("Bob", 30, "bob@example.com")

printf("Hello %s, you are %d years old", "Alice", 25)

processDataAsync({value = 100}, function(result, error)
    if error then
        print("Error:", error)
    else
        print("Result:", result)
    end
end, function(percent)
    print("Progress:", percent .. "%")
end)

httpRequest({
    method = "GET",
    url = "https://api.example.com/users",
    headers = {
        ["Authorization"] = "Bearer token123"
    }
}, {
    timeout = 60,
    retries = 5
})
```

## Features

1. **Optional parameter support**
2. **Union types**
3. **Generic parameters**
4. **Function types**
5. **Variadic parameters**
