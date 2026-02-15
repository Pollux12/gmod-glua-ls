# @module - Module Declaration

Declare module information and export types.

## Syntax

```lua
---@module [module_name]
```

## Examples

```lua
-- Basic module declaration
---@module utils

local utils = {}

---@param str string String to reverse
---@return string Reversed string
function utils.reverse(str)
    return string.reverse(str)
end

---@param arr table Array to shuffle
---@return table Shuffled array
function utils.shuffle(arr)
    local result = {}
    for i, v in ipairs(arr) do
        local j = math.random(1, i)
        result[i] = result[j]
        result[j] = v
    end
    return result
end

return utils

-- Named module with exports
---@module math_extensions

---@class Vector2
---@field x number
---@field y number
local Vector2 = {}

---@param x number X coordinate
---@param y number Y coordinate
---@return Vector2
function Vector2.new(x, y)
    return setmetatable({x = x, y = y}, {__index = Vector2})
end

---@param other Vector2 Other vector
---@return Vector2 Sum vector
function Vector2:add(other)
    return Vector2.new(self.x + other.x, self.y + other.y)
end

---@return number Vector magnitude
function Vector2:magnitude()
    return math.sqrt(self.x * self.x + self.y * self.y)
end

local math_extensions = {
    Vector2 = Vector2,
    PI2 = math.pi * 2,

    ---@param degrees number Angle in degrees
    ---@return number Angle in radians
    toRadians = function(degrees)
        return degrees * math.pi / 180
    end,

    ---@param radians number Angle in radians
    ---@return number Angle in degrees
    toDegrees = function(radians)
        return radians * 180 / math.pi
    end
}

return math_extensions

-- Database module
---@module database

---@class Connection
local Connection = {}

---@param query string SQL query
---@param params? table Query parameters
---@return table[] Results
function Connection:execute(query, params) end

---@return boolean Success
function Connection:close() end

---@class Database
local Database = {}

---@param config {host: string, port: number, database: string, user: string, password: string}
---@return Connection
function Database.connect(config) end

---@param query string SQL query
---@return table[] Results
function Database.query(query) end

return Database

-- Configuration module
---@module config

---@class AppConfig
---@field debug boolean Debug mode
---@field port number Server port
---@field database {host: string, name: string}
---@field features string[] Enabled features

---@type AppConfig
local defaultConfig = {
    debug = false,
    port = 8080,
    database = {
        host = "localhost",
        name = "myapp"
    },
    features = {"auth", "logging"}
}

---@param userConfig? Partial<AppConfig> User configuration
---@return AppConfig Merged configuration
local function createConfig(userConfig)
    local config = {}
    for k, v in pairs(defaultConfig) do
        config[k] = v
    end

    if userConfig then
        for k, v in pairs(userConfig) do
            config[k] = v
        end
    end

    return config
end

return {
    default = defaultConfig,
    create = createConfig
}

-- Event system module
---@module events

---@class EventEmitter
---@field private listeners table<string, fun(...)[]>
local EventEmitter = {}

function EventEmitter.new()
    return setmetatable({listeners = {}}, {__index = EventEmitter})
end

---@param event string Event name
---@param listener fun(...) Event listener
function EventEmitter:on(event, listener)
    if not self.listeners[event] then
        self.listeners[event] = {}
    end
    table.insert(self.listeners[event], listener)
end

---@param event string Event name
---@param ... any Event arguments
function EventEmitter:emit(event, ...)
    local eventListeners = self.listeners[event]
    if eventListeners then
        for _, listener in ipairs(eventListeners) do
            listener(...)
        end
    end
end

---@param event string Event name
---@param listener? fun(...) Specific listener to remove
function EventEmitter:off(event, listener)
    local eventListeners = self.listeners[event]
    if not eventListeners then return end

    if listener then
        for i, l in ipairs(eventListeners) do
            if l == listener then
                table.remove(eventListeners, i)
                break
            end
        end
    else
        self.listeners[event] = nil
    end
end

return {
    EventEmitter = EventEmitter
}
```

## Features

1. **Module identification**
2. **Export type definitions**
3. **Namespace organization**
4. **Dependency tracking**
5. **Documentation generation**
