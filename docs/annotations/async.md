# @async - Async Function Marker

Mark functions as asynchronous, used for async programming hints and type checking.

## Syntax

```lua
---@async
```

## Examples

```lua
-- Basic async function
---@async
---@param url string Request URL
---@return string Response content
function fetchData(url)
    -- Simulate async HTTP request
    return coroutine.wrap(function()
        print("Starting request:", url)
        -- Simulate network delay
        local co = coroutine.running()
        timer.setTimeout(function()
            coroutine.resume(co, "Response data: " .. url)
        end, 1000)
        return coroutine.yield()
    end)()
end

-- Async file operations
---@async
---@param filepath string File path
---@return string File content
function readFileAsync(filepath)
    return coroutine.wrap(function()
        local file = io.open(filepath, "r")
        if not file then
            error("Cannot open file: " .. filepath)
        end

        local content = file:read("*a")
        file:close()

        -- Simulate async reading
        coroutine.yield()
        return content
    end)()
end

---@async
---@param filepath string File path
---@param content string File content
---@return boolean Whether successful
function writeFileAsync(filepath, content)
    return coroutine.wrap(function()
        local file = io.open(filepath, "w")
        if not file then
            return false
        end

        file:write(content)
        file:close()

        -- Simulate async writing
        coroutine.yield()
        return true
    end)()
end

-- Async database operations
---@async
---@param query string SQL query
---@return table[] Query results
function queryDatabase(query)
    return coroutine.wrap(function()
        print("Executing query:", query)

        -- Simulate database connection and query
        local co = coroutine.running()
        database.execute(query, function(results)
            coroutine.resume(co, results)
        end)

        return coroutine.yield()
    end)()
end

-- Async error handling
---@async
---@param operation string Operation name
---@return boolean success
---@return string? error
function safeAsyncOperation(operation)
    return coroutine.wrap(function()
        local success, result = pcall(function()
            -- Simulate potentially failing async operation
            if operation == "fail" then
                error("Operation failed")
            end
            coroutine.yield()
            return "Operation completed"
        end)

        if success then
            return true, nil
        else
            return false, result
        end
    end)()
end

-- Usage examples
---@async
function main()
    -- Fetch data asynchronously
    local data = fetchData("https://api.example.com/users")
    print("Received:", data)

    -- Read file asynchronously
    local content = readFileAsync("config.txt")
    print("File content:", content)

    -- Write file asynchronously
    local success = writeFileAsync("output.txt", "Hello, World!")
    if success then
        print("File written successfully")
    end

    -- Query database asynchronously
    local users = queryDatabase("SELECT * FROM users")
    for _, user in ipairs(users) do
        print("User:", user.name)
    end

    -- Handle async operation with error handling
    local success, error = safeAsyncOperation("normal")
    if not success then
        print("Error:", error)
    end
end

-- Start the async main function
main()
```

## Features

1. **Async function marking**
2. **Coroutine integration**
3. **Error handling support**
4. **IDE async/await hints**
5. **Type system integration**
