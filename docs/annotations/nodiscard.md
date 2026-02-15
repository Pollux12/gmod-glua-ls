# @nodiscard - Non-discardable Return Value

Mark function return values that should not be ignored, ensuring important results are used.

## Syntax

```lua
---@nodiscard [reason]
```

## Examples

```lua
-- Basic nodiscard function
---@nodiscard
---@return boolean success
function validateInput(input)
    return input ~= nil and input ~= ""
end

-- Nodiscard with reason
---@nodiscard Must check for errors
---@param data string JSON data
---@return table? parsed_data
---@return string? error_message
function parseJSON(data)
    local success, result = pcall(json.decode, data)
    if success then
        return result, nil
    else
        return nil, result
    end
end

-- Critical operation result
---@nodiscard Critical: File operation result must be checked
---@param filepath string File path
---@param content string Content to write
---@return boolean success
---@return string? error
function writeFileSecure(filepath, content)
    local file, err = io.open(filepath, "w")
    if not file then
        return false, err
    end

    local success, writeErr = pcall(file.write, file, content)
    file:close()

    if not success then
        return false, writeErr
    end

    return true, nil
end

-- Database operation
---@nodiscard Database errors must be handled
---@param query string SQL query
---@return table[] results
---@return string? error
function executeQuery(query)
    local results, error = database.execute(query)
    if error then
        return {}, error
    end
    return results, nil
end

-- Memory allocation
---@nodiscard Memory allocation may fail
---@param size number Buffer size
---@return buffer? buffer
function allocateBuffer(size)
    if size <= 0 or size > MAX_BUFFER_SIZE then
        return nil
    end
    return buffer.create(size)
end

-- Usage examples (these will show warnings if results are ignored)

-- GOOD: Result is checked
local isValid = validateInput(userInput)
if not isValid then
    print("Invalid input")
end

-- GOOD: Error handling
local data, error = parseJSON(jsonString)
if error then
    print("Parse error:", error)
    return
end

-- GOOD: File operation result checked
local success, writeError = writeFileSecure("output.txt", "data")
if not success then
    print("Write failed:", writeError)
end

-- BAD: These will show warnings in IDE
validateInput(userInput)  -- Warning: Return value should not be discarded
parseJSON(jsonString)     -- Warning: Must check for errors
writeFileSecure("test.txt", "content")  -- Warning: Critical: File operation result must be checked
```

## Features

1. **Return value validation**
2. **Custom warning messages**
3. **IDE integration**
4. **Error handling enforcement**
5. **Critical operation safety**
