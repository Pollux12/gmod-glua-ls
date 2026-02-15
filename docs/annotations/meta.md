# @meta - Metadata File

Mark a file as containing only type definitions and metadata, not runtime code.

## Syntax

```lua
---@meta [library_name]
```

## Examples

```lua
-- Basic meta file
---@meta

-- Define types that exist at runtime but not in source
---@class string
---@field len fun(self: string): number
---@field sub fun(self: string, start: number, end?: number): string
---@field find fun(self: string, pattern: string): number?, number?

-- Library-specific meta file
---@meta json

---@class json
local json = {}

---@param str string JSON string
---@return table Parsed object
---@return nil, string Error message on failure
function json.decode(str) end

---@param obj table Object to encode
---@param pretty? boolean Pretty print
---@return string JSON string
function json.encode(obj, pretty) end

return json

-- Built-in function definitions
---@meta _G

---@param obj any Object to get type of
---@return string Type name
function type(obj) end

---@param func function Function to call
---@param ... any Arguments
---@return boolean success, ...
function pcall(func, ...) end

---@param co thread Coroutine
---@param ... any Values to pass
---@return boolean success, ...
function coroutine.resume(co, ...) end

-- External library definitions
---@meta socket

---@class socket
local socket = {}

---@class tcpsocket
local tcpsocket = {}

---@param address string IP address
---@param port number Port number
---@return boolean success, string? error
function tcpsocket:connect(address, port) end

---@param data string Data to send
---@return number? bytes_sent, string? error
function tcpsocket:send(data) end

---@return string? data, string? error
function tcpsocket:receive() end

function tcpsocket:close() end

---@return tcpsocket
function socket.tcp() end

return socket

-- Game engine meta definitions
---@meta love2d

---@class love
local love = {}

---@class love.graphics
love.graphics = {}

---@param text string Text to draw
---@param x number X position
---@param y number Y position
function love.graphics.print(text, x, y) end

---@param r number Red component (0-1)
---@param g number Green component (0-1)
---@param b number Blue component (0-1)
---@param a? number Alpha component (0-1)
function love.graphics.setColor(r, g, b, a) end

---@param mode string Draw mode
---@param ... number Coordinates
function love.graphics.polygon(mode, ...) end

-- ORM meta definitions
---@meta activerecord

---@class Model
local Model = {}

---@param attributes table Initial attributes
---@return Model
function Model.new(attributes) end

---@param conditions table Query conditions
---@return Model[]
function Model.where(conditions) end

---@param id number Record ID
---@return Model?
function Model.find(id) end

---@return boolean success
function Model:save() end

---@return boolean success
function Model:destroy() end

-- API client meta definitions
---@meta http_client

---@class HTTPClient
local HTTPClient = {}

---@param url string Request URL
---@param options? {headers?: table, timeout?: number}
---@return {status: number, body: string, headers: table}
function HTTPClient.get(url, options) end

---@param url string Request URL
---@param data table Request body
---@param options? {headers?: table, timeout?: number}
---@return {status: number, body: string, headers: table}
function HTTPClient.post(url, data, options) end

return HTTPClient
```

## Features

1. **Type-only file marking**
2. **Library type definitions**
3. **Runtime function signatures**
4. **External API definitions**
5. **Documentation generation**
