# Workspace Window And Metamodel

## Contents

- Workspace requests and notifications
- File operations
- Workspace symbols
- Window features
- Meta model

## Workspace requests and notifications

### Configuration

- `workspace/configuration`
- `workspace/didChangeConfiguration`

Rules:

- `workspace/configuration` is a pull model.
- Return results in the same order as the incoming `ConfigurationItem[]`; the response is always an array of the same length — if the client cannot provide a configuration setting for a given scope, `null` needs to be present in that position.
- If the server caches pulled configuration, it should register for empty `didChangeConfiguration` notifications so it knows when to invalidate the cache.

### Workspace folders

- `workspace/workspaceFolders`
- `workspace/didChangeWorkspaceFolders`

Rules:

- `workspace/workspaceFolders` is gated by the client capability `workspace.workspaceFolders: boolean`. The server advertises workspace folder support by declaring `workspace.workspaceFolders.supported: true` in its server capabilities (`WorkspaceFoldersServerCapabilities`).
- `workspace/workspaceFolders` returns:
  - `null` when only a single file is open
  - `[]` when a workspace is open but has no folders
- Server interest in folder-change notifications is expressed through the workspace folder capability and change-notification settings.

### Watched files

- `workspace/didChangeWatchedFiles`

Rules:

- Watched changes cover filesystem events for files and folders.
- The spec recommends client-side watching because server-side watching is expensive and hard to implement portably.
- 3.17 adds relative pattern support for file watchers.
- Watch patterns may be plain glob strings or `RelativePattern`.
- If watcher `kind` is omitted, it defaults to create, change, and delete.

### Execute command and apply edit

- `workspace/executeCommand`
- `workspace/applyEdit`

Rules:

- `workspace/executeCommand` is advertised by the server through `executeCommandProvider`; the client capability `workspace.executeCommand` controls dynamic registration only, not whether the feature is available.
- The server advertises execution support through `executeCommandProvider`.
- `executeCommandProvider.commands` lists the command identifiers the server can execute.
- `workspace.executeCommand.dynamicRegistration` governs whether execute-command support can be registered dynamically.
- `workspace/executeCommand` params carry the command identifier plus optional `arguments`.
- `workspace/executeCommand` returns `LSPAny` and uses standard error code/message fields on failure.
- `workspace/applyEdit` is gated by the client `workspace.applyEdit` capability.
- `workspace/applyEdit` request params carry an optional UI label and the `WorkspaceEdit` to apply.
- `workspace/applyEdit` returns:
  - `applied`
  - optional `failureReason`
  - optional `failedChange` (only when the client has signaled a `failureHandling` strategy via `workspace.workspaceEdit.failureHandling` in client capabilities)

## File operations

- Requests:
  - `workspace/willCreateFiles`
  - `workspace/willRenameFiles`
  - `workspace/willDeleteFiles`
- Notifications:
  - `workspace/didCreateFiles`
  - `workspace/didRenameFiles`
  - `workspace/didDeleteFiles`

Rules:

- `will*` results (the returned `WorkspaceEdit`) may be dropped by clients if computing the edit took too long or if the server repeatedly fails; the file operation still proceeds regardless.
- Apply the returned `WorkspaceEdit` before the actual file operation.
- `willCreateFiles` edits cannot manipulate the contents of the files being created.
- For folder rename, `willRenameFiles` reports the renamed folder, not every child.
- File-operation interest is gated by client `workspace.fileOperations.*` capabilities and server registration options.

## Workspace symbols

- `workspace/symbol`
- `workspaceSymbol/resolve`

Rules:

- `workspace/symbol` may return `SymbolInformation[]`, `WorkspaceSymbol[]`, or `null`, according to the negotiated capabilities.
- Servers may only use the deferred location model (returning `WorkspaceSymbol` with a URI-only location) if the client advertises `workspace.symbol.resolveSupport`.
- The server-side resolve model is advertised with `workspaceSymbolProvider.resolveProvider`.
- Do not rely on deferred location detail if that support is absent.

## Window features

- `window/showDocument`
- `window/workDoneProgress/create`
- `window/workDoneProgress/cancel`
- `window/showMessage`
- `window/showMessageRequest`
- `window/logMessage`
- `telemetry/event`

Rules:

- `window/showDocument` takes a required URI, an optional `external` boolean that indicates the resource should be shown in an external program, an optional `takeFocus` hint that clients may ignore if an external program is started, and an optional `selection` range that clients may ignore if an external program is started or the file is not a text file.
- `window/showDocument` returns `ShowDocumentResult { success: boolean }` indicating whether the show was successful.
- `window/showDocument` is gated by the `window.showDocument` client capability; its `support` field must be `true`.
- `window/workDoneProgress/create` requires the client `window.workDoneProgress` capability.
- If progress creation fails, the server must not emit progress for that token.
- `window/workDoneProgress/cancel` is a valid client cancellation signal even if the original progress was not marked cancellable.
- `window/showMessage` is a server-to-client notification with `type` and `message`.
- `window/logMessage` is a server-to-client notification with `type` and `message`.
- `window/showMessageRequest` is a server-to-client request with `type`, `message`, and optional `actions`, and it returns the selected `MessageActionItem` or `null`.
- `window.showMessage.messageActionItem.additionalPropertiesSupport` controls whether extra attributes on action items are preserved in the response.
- `telemetry/event` is a server-to-client notification whose payload is intentionally protocol-opaque `LSPAny` (which may be an object, array, string, number, boolean, or null).
- `MessageType` values used by show/log message traffic in 3.17 are:
  - `Error = 1`
  - `Warning = 2`
  - `Info = 3`
  - `Log = 4`
- The raw upstream include also mentions `Debug = 5` as a 3.18 proposed value; do not rely on it for 3.17 work.
- During initialize, the server may send `window/showMessage`, `window/logMessage`, `telemetry/event`, and `window/showMessageRequest` before the initialize response, plus `$/progress` only for the initialize token if the client provided a `workDoneToken` in `InitializeParams`.

## Meta model

- LSP 3.17 includes a machine-readable meta model.
- Key files in the spec distribution:
  - `metaModel.json`
  - `metaModel.ts`
  - `metaModel.schema.json`
- Use the meta model as a code generation or validation aid for protocol structures, requests, notifications, enumerations, and aliases.
- Preserve the distinctions encoded by the meta model:
  - requests
  - notifications
  - structures
  - enumerations
  - type aliases
  - metadata
