# @enum - Enumeration Definition

Mark a Lua table as an enumeration type, providing runtime-available enumeration values.

## Syntax

```lua
-- Value enumeration (using table values)
---@enum <enum_name>

-- Key enumeration (using table keys)
---@enum (key) <enum_name>
```

## Examples

```lua
-- Basic value enumeration
---@enum HTTPStatus
local HTTPStatus = {
    OK = 200,
    NOT_FOUND = 404,
    INTERNAL_ERROR = 500,
    BAD_REQUEST = 400,
    UNAUTHORIZED = 401
}

-- String value enumeration
---@enum LogLevel
local LogLevel = {
    DEBUG = "debug",
    INFO = "info",
    WARN = "warn",
    ERROR = "error",
    FATAL = "fatal"
}

-- Key enumeration (using table keys as enumeration values)
---@enum (key) Permission
local Permission = {
    READ = true,
    WRITE = true,
    DELETE = true,
    ADMIN = true
}

-- Mixed type enumeration
---@enum TaskStatus
local TaskStatus = {
    PENDING = 0,
    RUNNING = "running",
    COMPLETED = true,
    FAILED = false
}

-- Functions using enumerations
---@param status HTTPStatus HTTP status code
---@return string Status description
function getStatusMessage(status)
    if status == HTTPStatus.OK then
        return "Request successful"
    elseif status == HTTPStatus.NOT_FOUND then
        return "Resource not found"
    elseif status == HTTPStatus.INTERNAL_ERROR then
        return "Internal server error"
    else
        return "Unknown status"
    end
end

---@param level LogLevel Log level
---@param message string Log message
function writeLog(level, message)
    local timestamp = os.date("%Y-%m-%d %H:%M:%S")
    print(string.format("[%s] %s: %s", timestamp, level, message))
end

---@param user table User object
---@param permission Permission Permission type
---@return boolean Whether has permission
function hasPermission(user, permission)
    return user.permissions and user.permissions[permission]
end

-- Usage examples
local response = {
    status = HTTPStatus.OK,
    data = {message = "Hello World"}
}

print(getStatusMessage(response.status))

writeLog(LogLevel.INFO, "Application started")
writeLog(LogLevel.ERROR, "Database connection failed")

local currentUser = {
    id = 1001,
    name = "John",
    permissions = {
        [Permission.READ] = true,
        [Permission.WRITE] = true
    }
}

if hasPermission(currentUser, Permission.WRITE) then
    print("User can write")
end

-- Enumeration iteration
print("Available log levels:")
for name, value in pairs(LogLevel) do
    print(string.format("  %s = %s", name, value))
end

-- Type checking with enumerations
---@param status TaskStatus Task status
function handleTaskStatus(status)
    if status == TaskStatus.PENDING then
        print("Task is waiting to start")
    elseif status == TaskStatus.RUNNING then
        print("Task is currently running")
    elseif status == TaskStatus.COMPLETED then
        print("Task completed successfully")
    elseif status == TaskStatus.FAILED then
        print("Task failed to complete")
    end
end

handleTaskStatus(TaskStatus.RUNNING)
```

## Features

1. **Runtime enumeration values**
2. **Key and value enumerations**
3. **Mixed type support**
4. **Type checking**
5. **Iteration support**
