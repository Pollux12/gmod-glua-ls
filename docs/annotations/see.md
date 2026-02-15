# @see - Reference to Other Symbols

Create references to other symbols, classes, functions, or external documentation.

## Syntax

```lua
---@see <reference>
```

## Examples

```lua
-- Reference to other functions
---@param data string Input data
---@return string Processed data
---@see validateData For data validation
---@see formatOutput For output formatting
function processData(data)
    local validated = validateData(data)
    return formatOutput(validated)
end

-- Reference to classes
---@class User
---@field id number
---@field name string
---@see UserManager For user management operations
---@see UserValidator For user data validation
local User = {}

-- Reference to related methods
---@class Database
local Database = {}

---@param query string SQL query
---@return table[] Query results
---@see Database:execute For direct query execution
---@see Database:prepare For prepared statements
function Database:select(query)
    return self:execute("SELECT " .. query)
end

---@param table string Table name
---@param data table Data to insert
---@return boolean Success
---@see Database:select For data retrieval
---@see Database:update For data modification
function Database:insert(table, data)
    local query = buildInsertQuery(table, data)
    return self:execute(query)
end

-- Reference to external documentation
---@param config table Configuration object
---@return HTTPClient Client instance
---@see https://example.com/docs/http-client HTTP Client Documentation
---@see README.md#configuration Configuration Guide
function createHTTPClient(config)
    return HTTPClient.new(config)
end

-- Reference to interfaces and protocols
---@class EventEmitter
---@see Comparable For comparison operations
---@see Serializable For serialization support
local EventEmitter = {}

-- Multiple related references
---@param user User User object
---@param permissions table Permissions table
---@return boolean Has permission
---@see User For user object structure
---@see PermissionManager For permission management
---@see SecurityPolicy For security guidelines
---@see audit.log For permission check logging
function checkPermission(user, permissions)
    return PermissionManager.validate(user, permissions)
end

-- Reference to error handling
---@param operation function Operation to execute
---@return any result
---@return string? error
---@see ErrorHandler For error processing
---@see Logger For error logging
---@see RetryPolicy For retry strategies
function safeExecute(operation)
    local success, result = pcall(operation)
    if success then
        return result, nil
    else
        ErrorHandler.process(result)
        return nil, result
    end
end

-- Reference to design patterns
---@class Factory
---@see Builder For complex object construction
---@see Singleton For single instance management
---@see Observer For event notification patterns
local Factory = {}

-- Reference to algorithms
---@param array table Array to sort
---@return table Sorted array
---@see quickSort For large datasets
---@see mergeSort For stable sorting
---@see insertionSort For small datasets
function bubbleSort(array)
    -- Implementation
    return array
end

-- Reference to related configurations
---@class AppConfig
---@field database table Database configuration
---@field server table Server configuration
---@see config/database.lua For database settings
---@see config/server.lua For server settings
---@see docs/configuration.md For configuration guide
local AppConfig = {}

-- Cross-module references
---@module utils
---@see string_utils For string manipulation functions
---@see math_utils For mathematical utilities
---@see file_utils For file system operations

-- Reference to test cases
---@param input string Input to validate
---@return boolean Is valid
---@see tests/validation_test.lua For test cases
---@see spec/validation.spec For behavior specification
function validateInput(input)
    return input ~= nil and #input > 0
end

-- Reference to API endpoints
---@param userId number User ID
---@return User User data
---@see GET /api/users/:id API endpoint
---@see UserController.show Controller method
function getUser(userId)
    return api.get("/users/" .. userId)
end

-- Reference to standards and specifications
---@param jwt string JWT token
---@return table Decoded payload
---@see RFC 7519 JSON Web Token specification
---@see https://jwt.io/ JWT debugging tools
function decodeJWT(jwt)
    return jwt.decode(jwt)
end
```

## Features

1. **Symbol cross-referencing**
2. **Documentation linking**
3. **API endpoint references**
4. **External resource linking**
5. **Code navigation aids**
