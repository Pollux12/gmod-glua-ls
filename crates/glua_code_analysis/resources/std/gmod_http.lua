---@meta no-require

---@alias HTTPMethod
---| "GET"
---| "POST"
---| "HEAD"
---| "PUT"
---| "DELETE"
---| "PATCH"
---| "OPTIONS"

---@class HTTPRequest
---@field url string Target URL.
---@field method HTTPMethod HTTP method. Default: "GET"
---@field success fun(code: number, body: string, headers: table) Called on success. `code` is HTTP status, `body` is response content, `headers` are response headers.
---@field failed fun(reason: string) Called on failure with the reason string.
---@field parameters table<string, string>? Key-value URL parameters (GET/POST/HEAD only; ignored if `body` is set).
---@field headers table<string, string>? Key-value request headers.
---@field body string? POST body. Overrides `parameters` when set.
---@field type string? Content-Type for `body`. Default: "text/plain; charset=utf-8"
---@field timeout number? Connection timeout in seconds. Default: 60

---@param req HTTPRequest
function HTTP(req) end
