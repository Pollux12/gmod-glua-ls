# @alias - Type Alias

Define type aliases for creating custom types, enumeration types, or simplifying complex type expressions.

## Syntax

```lua
-- Simple alias
---@alias <alias> <type_expression>

-- Enumeration alias
---@alias <alias>
---| '<value1>' [# description1]
---| '<value2>' [# description2]
---| ...

-- Generic alias
---@alias <alias><<generic_parameter_list>> <type_expression>
```

## Examples

```lua
-- Simple type aliases
---@alias ID number
---@alias UserName string

-- Union type aliases
---@alias StringOrNumber string | number
---@alias MaybeString string | nil

-- Enumeration type aliases
---@alias HTTPMethod
---| 'GET'     # HTTP GET request
---| 'POST'    # HTTP POST request
---| 'PUT'     # HTTP PUT request
---| 'DELETE'  # HTTP DELETE request
---| 'PATCH'   # HTTP PATCH request

-- Status enumeration
---@alias TaskStatus
---| 'pending'   # Waiting for execution
---| 'running'   # Currently executing
---| 'completed' # Execution completed
---| 'failed'    # Execution failed

-- Generic aliases
---@alias Result<T, E> {success: boolean, data: T, error: E}
---@alias Array<T> T[]
---@alias Dictionary<K, V> table<K, V>

-- Complex function type aliases
---@alias EventHandler fun(event: string, ...): boolean
---@alias AsyncCallback<T> fun(error: string?, result: T?): nil

-- Usage examples
---@type HTTPMethod
local method = 'GET'

---@type Result<string, string>
local result = {success = true, data = "Hello", error = nil}

---@param status TaskStatus
function updateTaskStatus(status)
    print("Task status:", status)
end

updateTaskStatus('running')
```

## Use Cases

1. **Simplifying complex type expressions**
2. **Creating enumeration types**
3. **Defining common data structures**
4. **Improving code readability and maintainability**
