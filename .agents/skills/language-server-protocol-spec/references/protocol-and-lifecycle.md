# Protocol And Lifecycle

## Contents

- Base protocol
- JSON-RPC message rules
- Error codes
- Cancellation and progress
- Initialize and capability exchange
- Dynamic registration
- Shutdown and exit
- Implementation considerations

## Base protocol

- Frame messages as an ASCII header block followed by `\r\n\r\n`, then a JSON content body.
- Each header field is a `name: value` pair terminated by `\r\n`, conforming to HTTP header semantics (RFC 7230 section 3.2).
- Require `Content-Length`.
- Allow `Content-Type`; default it to `application/vscode-jsonrpc; charset=utf-8`.
- If a receiver encounters a header encoding other than `utf-8`, it should respond with an error.
- Encode the content body as UTF-8. The spec recommends treating legacy `utf8` as `utf-8` for compatibility.
- Use JSON-RPC 2.0 with `jsonrpc: "2.0"`.

## JSON-RPC message rules

- Requests always carry an `id` (integer or string), `method`, and optional `params`.
- Every processed request must receive a response.
- If the request succeeds but has no meaningful payload, return `result: null`.
- Notifications never receive a response and must not include an `id` field.
- Response messages carry either `result` or `error`, never both; `result` must not exist if there was an error invoking the method.
- `ResponseMessage.id` may be `null` when the original request id cannot be determined (e.g., in error responses).
- `$/` notifications received by either side may be ignored.
- `$/` requests received by either a server or client must fail with `MethodNotFound`.
- LSP convention prefers object params for standard protocol messages, though custom messages may use arrays.

## Error codes

- JSON-RPC base errors:
  - `ParseError = -32700`
  - `InvalidRequest = -32600`
  - `MethodNotFound = -32601`
  - `InvalidParams = -32602`
  - `InternalError = -32603`
- Initialization and protocol-state errors:
  - `ServerNotInitialized = -32002`
  - `UnknownErrorCode = -32001`
- LSP-reserved request-state errors:
  - `RequestFailed = -32803` — a request failed but was syntactically correct (method known, params valid)
  - `ServerCancelled = -32802` — may only be used for requests that explicitly support being server-cancellable
  - `ContentModified = -32801`
  - `RequestCancelled = -32800`
- Reserved error code ranges (no LSP error codes should be defined between start and end):
  - JSON-RPC reserved range: `-32099` to `-32000`
  - LSP reserved range: `-32899` to `-32800`
- Deprecated identifiers: `serverErrorStart` and `serverErrorEnd` are deprecated names for `jsonrpcReservedErrorRangeStart` and `jsonrpcReservedErrorRangeEnd` respectively.

## Cancellation and progress

- Support request cancellation via `$/cancelRequest`.
- A canceled request still has to complete with a response; it must not hang open.
- If a request is canceled and returns an error, the recommended code is `RequestCancelled`.
- Report generic progress via `$/progress` using a progress token that is separate from the request id.
- Create server-initiated work-done progress with `window/workDoneProgress/create` before sending progress for that token.
- Only use `window/workDoneProgress/create` when the client advertises `window.workDoneProgress`.
- If `window/workDoneProgress/create` fails, the server must not send progress notifications for that token.
- Each server-initiated progress token should be used for exactly one progress session (one begin, zero or more reports, one end).
- `window/workDoneProgress/cancel` may be sent even when the progress item was not marked cancellable; a client may cancel for any reason (e.g., error, workspace reload).
- Use feature-specific work-done progress and partial-result support only where the feature declares it.
- If a request errors with `RequestCancelled`, clients are free to use any provided partial results but should make clear that the request was cancelled and results may be incomplete; for all other errors, partial results should not be used.
- If clients receive a `ContentModified` error, they generally should not show it in the UI for the end-user.

## Initialize and capability exchange

- The client sends `initialize` exactly once and first.
- If the server receives a request before `initialize`, it should answer with `ServerNotInitialized`.
- If the server receives a notification before `initialize`, it should drop it, except for `exit`.
- Before the server answers `initialize`, the client must not send additional requests or notifications.
- Before the server answers `initialize`, the server must not send requests or notifications except:
  - `window/showMessage`
  - `window/logMessage`
  - `telemetry/event`
  - `window/showMessageRequest`
  - `$/progress` only for the initialize work-done token if the client supplied one
- After a successful initialize response, the client sends `initialized` once.
- Exchange client and server capabilities during `initialize`.
- Treat missing 3.x client capability properties as absence of the corresponding capability. If a missing property normally defines sub properties, all missing sub properties should also be treated as absent.
- Protocol features that existed in 2.x (e.g., text document synchronization for open, change, close) remain mandatory for clients; clients cannot opt out of providing them.
- Servers should ignore unknown client capability properties for future compatibility.
- Clients should ignore unknown server capability fields instead of failing initialize.
- `InitializeError` (returned in `error.data`) includes a required `retry: boolean` field. When `true`, the client shows the error message to the user, presents retry/cancel options, and re-sends `initialize` if the user retries.
- `InitializeErrorCodes` includes the deprecated value `unknownProtocolVersion = 1`.
- The server capability `positionEncoding` is negotiated against `general.positionEncodings`.
- If the client omits `general.positionEncodings`, it defaults to `['utf-16']`; treat UTF-16 as mandatory. If the client sends the array but `utf-16` is absent from it, servers can safely assume UTF-16 support (UTF-16 is a mandatory encoding regardless).

## Dynamic registration

- Use dynamic registration only when the feature-specific client capability advertises `dynamicRegistration`.
- Register client-side capabilities with `client/registerCapability`.
- Unregister them with `client/unregisterCapability`.
- The server must not register the same capability both statically through the initialize result and dynamically for the same document selector. If the client does not support dynamic registration for a capability, register it statically; otherwise register it dynamically.

## Shutdown and exit

- `shutdown` is a client-to-server request with no params and a `null` result on success.
- After a shutdown request, the client must not send further requests and must not send notifications other than `exit`.
- The client should wait for the shutdown response before sending `exit`.
- If the server receives requests after shutdown, it should answer with `InvalidRequest`.
- `exit` is a notification.
- The server should exit with code `0` if shutdown was received first; otherwise exit with code `1`.

## Implementation considerations

- Prefer returning responses in roughly request order; reordering is only permitted when it does not affect correctness.
- Do not cancel work only because newer messages are queued; the older result may still be useful.
- Use `ContentModified` only when the server's own internal state invalidates an in-flight result.
- If the client no longer needs a result, the client should cancel the request.
- Clients should not send resolve requests for stale objects.
- If a resolve request is stale, the server may answer with `ContentModified`.
- The spec assumes one server serves one tool.
- If a client notices that a server exits unexpectedly, it should try to restart the server. However clients should be careful not to restart a crashing server endlessly (e.g., VS Code does not restart a server that has crashed 5 times in the last 180 seconds).
- Clients can resend requests after receiving `ContentModified` if they know how to do so.
- The spec recommends supporting these transport CLI modes when applicable:
  - `--stdio`
  - `--pipe` (uses pipes on Windows or socket files on Linux/Mac; path passed as next arg or with `--pipe=<path>`)
  - `--socket` (port passed as next arg or with `--port=<number>`)
  - `--node-ipc` (only supported if both client and server run under Node)
- Pass the client process id to the server via `--clientProcessId`, matching `initialize.processId`. This allows the server to monitor the editor process and exit if the parent process dies.
