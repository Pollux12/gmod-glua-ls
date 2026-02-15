# @deprecated - Deprecation Marker

Mark functions, classes, or fields as deprecated, reminding developers to use alternative solutions.

## Syntax

```lua
---@deprecated [alternative_explanation]
```

## Examples

```lua
-- Simple deprecation marker
---@deprecated
function oldFunction()
    print("This is an old function")
end

-- Deprecation with alternative explanation
---@deprecated Please use newCalculateSum function instead
---@param numbers number[] Array of numbers
---@return number Sum
function calculateSum(numbers)
    local sum = 0
    for _, num in ipairs(numbers) do
        sum = sum + num
    end
    return sum
end

-- New replacement function
---@param numbers number[] Array of numbers
---@return number Sum
function newCalculateSum(numbers)
    return table.reduce(numbers, function(acc, num) return acc + num end, 0)
end

-- Deprecated class
---@deprecated Please use ModernUser class instead
---@class OldUser
---@field id number
---@field name string
local OldUser = {}

-- New replacement class
---@class ModernUser
---@field id number
---@field name string
---@field email string
---@field createdAt string
local ModernUser = {}

-- Deprecated field
---@class APIResponse
---@field success boolean
---@field data any
---@field message string
---@deprecated Please use errorMessage field instead
---@field error string

-- Deprecated method
---@class FileManager
local FileManager = {}

---@deprecated Use readFileSync or readFileAsync instead
---@param path string File path
---@return string File content
function FileManager:loadFile(path)
    local file = io.open(path, "r")
    if file then
        local content = file:read("*a")
        file:close()
        return content
    end
    return ""
end

---@param path string File path
---@return string File content
function FileManager:readFileSync(path)
    local file = io.open(path, "r")
    if not file then
        error("Could not open file: " .. path)
    end
    local content = file:read("*a")
    file:close()
    return content
end

-- Deprecated with version information
---@deprecated Since v2.0.0, use authenticateUser instead
---@param username string Username
---@param password string Password
---@return boolean Whether login succeeded
function login(username, password)
    -- Old authentication logic
    return username == "admin" and password == "secret"
end

-- Conditional deprecation
---@deprecated Only use for legacy compatibility, migrate to new API
---@param data table Legacy data format
---@return table Converted data
function convertLegacyData(data)
    print("WARNING: Using deprecated convertLegacyData function")
    -- Conversion logic
    return {
        id = data.old_id,
        name = data.old_name,
        type = "legacy"
    }
end

-- Usage warnings
-- These will show deprecation warnings in IDE
local result1 = oldFunction()  -- Warning: Function is deprecated
local result2 = calculateSum({1, 2, 3})  -- Warning: Please use newCalculateSum function instead

local user = OldUser.new()  -- Warning: Please use ModernUser class instead
```

## Features

1. **Simple deprecation marking**
2. **Alternative solution guidance**
3. **IDE warning integration**
4. **Documentation generation support**
5. **Version-aware deprecation**
