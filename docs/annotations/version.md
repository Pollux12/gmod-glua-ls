# @version - Version Requirements

Specify version requirements for APIs, features, or compatibility.

## Syntax

```lua
---@version <version_spec> [description]
```

## Examples

```lua
-- Minimum version requirement
---@version >=1.2.0
function modernFunction()
    -- This function requires version 1.2.0 or higher
    return "Available in v1.2.0+"
end

-- Exact version requirement
---@version =1.0.0
function legacyFunction()
    -- This function only works in exactly version 1.0.0
    return "Only in v1.0.0"
end

-- Version range
---@version >=1.0.0,<2.0.0
function stableAPIFunction()
    -- Works in version 1.x series only
    return "Stable API v1.x"
end

-- Multiple version constraints
---@version >=1.5.0,!=1.6.0,<2.0.0
function patchedFunction()
    -- Available from 1.5.0, but not 1.6.0 (buggy), until 2.0.0
    return "Available with patch"
end

-- Lua version requirements
---@version Lua>=5.3
function utf8Function()
    -- Requires Lua 5.3+ for UTF-8 support
    return utf8.len("Hello 世界")
end

---@version LuaJIT>=2.1
function jitOptimizedFunction()
    -- Optimized for LuaJIT 2.1+
    local ffi = require("ffi")
    return ffi.new("int", 42)
end

-- Engine version requirements
---@version LÖVE>=11.0
function love2dNewFeature()
    -- Requires LÖVE 11.0+ for new graphics features
    love.graphics.setColor(1, 1, 1, 1)  -- New color format
end

---@version World of Warcraft>=8.0.0
function wowAPIFunction()
    -- WoW addon API function for Battle for Azeroth+
    return C_Map.GetMapInfo(1)
end

-- Deprecated in version
---@version <2.0.0
---@deprecated Removed in v2.0.0, use newFunction instead
function oldFunction()
    return "Will be removed"
end

-- Added in version
---@version >=1.3.0 Added support for async operations
function asyncFunction()
    -- New feature added in version 1.3.0
    return coroutine.create(function() end)
end

-- Class with version requirements
---@version >=2.1.0
---@class ModernClass
---@field property string Available since v2.1.0
local ModernClass = {}

---@version >=2.2.0 Enhanced with new options
---@param options table Configuration options (v2.2.0+)
function ModernClass:configure(options)
    -- Enhanced method available since v2.2.0
    self.options = options
end

-- API compatibility versions
---@version API>=3.0 RESTful API v3.0+
---@param endpoint string API endpoint
---@return table Response data
function apiCall(endpoint)
    return http.get("/api/v3/" .. endpoint)
end

-- Platform version requirements
---@version Windows>=10
---@version macOS>=10.15
---@version Linux>=Ubuntu18.04
function crossPlatformFunction()
    -- Requires modern operating systems
    return os.execute("modern-command")
end

-- Dependency version requirements
---@version socket>=3.0
---@version json>=1.2.0
function networkFunction()
    local socket = require("socket")
    local json = require("json")

    local client = socket.tcp()
    local data = json.encode({message = "hello"})
    return client:send(data)
end

-- Beta/Alpha version support
---@version >=2.0.0-beta.1 Experimental feature
function experimentalFunction()
    -- Available in beta versions
    return "Experimental feature"
end

---@version >=1.0.0-alpha.3,<1.0.0 Alpha testing only
function alphaFunction()
    -- Only available during alpha testing
    return "Alpha feature"
end

-- Security version requirements
---@version >=1.4.2 Security patch required
function secureFunction()
    -- Requires security patch from v1.4.2
    return "Secure implementation"
end

-- Performance version requirements
---@version >=3.1.0 Optimized implementation
function optimizedFunction(data)
    -- Optimized version available since 3.1.0
    return processDataFast(data)
end

-- Feature flag versions
---@version >=2.0.0 Requires feature flag: NEW_UI
function newUIFunction()
    -- Requires both version and feature flag
    if not featureFlags.NEW_UI then
        error("NEW_UI feature flag required")
    end
    return "New UI component"
end

-- Conditional version behavior
---@param input string Input data
---@return string Processed output
function adaptiveFunction(input)
    ---@version >=1.5.0
    if version.current >= "1.5.0" then
        return advancedProcess(input)
    else
        ---@version >=1.0.0,<1.5.0
        return basicProcess(input)
    end
end
```

## Features

1. **Version constraint specification**
2. **Compatibility documentation**
3. **Deprecation tracking**
4. **Platform requirements**
5. **Dependency management**
