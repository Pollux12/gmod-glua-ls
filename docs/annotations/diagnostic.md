# @diagnostic - Diagnostic Control

Control diagnostic warnings and errors for specific code sections.

## Syntax

```lua
---@diagnostic <action>: <diagnostic_name>[, <diagnostic_name>...]
```

## Actions

- `disable` - Disable diagnostics for the next line
- `disable-next-line` - Disable diagnostics for the next line
- `disable-line` - Disable diagnostics for the current line

## Examples

```lua
-- Disable specific warning for next line
---@diagnostic disable-next-line: undefined-global
print(someUndefinedVariable)

-- Disable multiple warnings
---@diagnostic disable-next-line: undefined-global, lowercase-global
GLOBAL_CONSTANT = someUndefinedValue

-- Disable unused variable warning
---@diagnostic disable-next-line: unused-local
local unusedVariable = "this won't be used"

-- Disable type mismatch warning
---@param value string
function processString(value)
    ---@diagnostic disable-next-line: param-type-mismatch
    processString(123)  -- Passing number instead of string
end

-- Disable undefined field warning
---@class User
---@field name string
local user = {}

---@diagnostic disable-next-line: undefined-field
local email = user.email  -- email field not defined in User class

-- Disable missing return warning
---@return string
function getName()
    ---@diagnostic disable-next-line: missing-return
    -- Function doesn't return anything but should return string
end

-- Disable redundant parameter warning
---@diagnostic disable-next-line: redundant-parameter
string.format("%s", "hello", "extra parameter")

-- Disable deprecated usage warning
---@deprecated Use newFunction instead
function oldFunction()
    return "old implementation"
end

---@diagnostic disable-next-line: deprecated
local result = oldFunction()

-- Block-level diagnostic control
do
    ---@diagnostic disable: undefined-global
    someGlobalVar = "this is OK"
    anotherGlobalVar = 42
    ---@diagnostic enable: undefined-global
end

-- File-level diagnostic control (usually at top of file)
---@diagnostic disable: lowercase-global

GLOBAL_CONFIG = {
    debug = true,
    version = "1.0.0"
}

-- Disable specific warnings for dynamic code
---@diagnostic disable-next-line: undefined-field
local dynamicField = obj[computedFieldName]

-- Disable for metatable operations
local mt = {
    __index = function(t, k)
        ---@diagnostic disable-next-line: undefined-field
        return rawget(t, k) or defaultValue
    end
}

-- Disable for external library usage
---@diagnostic disable-next-line: undefined-global
local json = require("external-json-lib")

-- Common diagnostic names:
-- - undefined-global: Undefined global variable
-- - lowercase-global: Global variable should be uppercase
-- - unused-local: Unused local variable
-- - unused-function: Unused function
-- - param-type-mismatch: Parameter type mismatch
-- - return-type-mismatch: Return type mismatch
-- - undefined-field: Undefined field access
-- - missing-return: Missing return statement
-- - redundant-parameter: Redundant function parameter
-- - deprecated: Usage of deprecated symbol
```

## Features

1. **Selective warning control**
2. **Line-specific disabling**
3. **Block-level control**
4. **Multiple diagnostic handling**
5. **Fine-grained error management**
