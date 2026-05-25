# Shared Types And Sync

## Contents

- Core text-document structures
- Text edits and workspace edits
- Text document synchronization
- Save hooks
- Text document rename signaling
- Notebook synchronization

## Core text-document structures

- `TextDocumentItem` carries `uri`, `languageId`, `version`, and full `text`.
- `TextDocumentIdentifier` identifies a document by `uri`.
- `VersionedTextDocumentIdentifier` adds a required `version`.
- `OptionalVersionedTextDocumentIdentifier` allows `version: integer | null`.
- Use the synchronized document state, not filesystem reads, for open documents.

## Text edits and workspace edits

- `TextEdit` describes a `range` and replacement `newText`.
- `AnnotatedTextEdit` extends text edits with change annotations.
- `WorkspaceEdit` may use either:
  - `changes` — a map of `DocumentUri` to `TextEdit[]`
  - `documentChanges` — either a plain `TextDocumentEdit[]`, or (when the client advertises `workspace.workspaceEdit.resourceOperations`) a mixed array of `TextDocumentEdit` entries interleaved with resource operations (`create`, `rename`, `delete`)
  - `changeAnnotations` — a map of annotation identifiers to `ChangeAnnotation` objects, used by annotated text edits
- A well-formed `WorkspaceEdit` should provide either `changes` or `documentChanges`, not both.
- If `documentChanges` are present and the client can handle versioned document edits, clients prefer `documentChanges` over `changes`.
- Execute `WorkspaceEdit.documentChanges` in the listed order (e.g., a create must precede an edit on the same file).
- Resource operations inside `documentChanges` are `create`, `rename`, and `delete`.
- File resource operations apply to files and folders.
- For create and rename options, `overwrite` wins over `ignoreIfExists`.
- Respect client workspace-edit capabilities:
  - `workspace.workspaceEdit.documentChanges`
  - `workspace.workspaceEdit.resourceOperations`
  - `workspace.workspaceEdit.failureHandling`
  - `workspace.workspaceEdit.normalizesLineEndings`
  - `workspace.workspaceEdit.changeAnnotationSupport`

## Text document synchronization

- `textDocument/didOpen`, `textDocument/didChange`, and `textDocument/didClose` are mandatory client support; this includes both full and incremental synchronization in `textDocument/didChange`.
- A server must implement all three or none.
- `didOpen` and `didClose` must stay balanced; do not open the same document twice without a close. A `didClose` notification requires a prior `didOpen` to have been sent for the same document.
- `didOpen` means the client owns document contents while open.
- `didClose` hands content ownership back to the URI target.
- A document being open or closed does not by itself forbid server requests about it.
- Advertise sync through `textDocumentSync` as either:
  - `TextDocumentSyncKind` — simple enum for sync mode
  - `TextDocumentSyncOptions` — structured object with optional properties `openClose`, `change`, `willSave`, `willSaveWaitUntil`, and `save` (typed `boolean | SaveOptions`, where `SaveOptions.includeText` controls whether full document text is included in `didSave`) for granular control over each sync behavior
- `TextDocumentSyncKind` values:
  - `None = 0`
  - `Full = 1`
  - `Incremental = 2`
- `didChange` content changes are applied in order.
- The version on `VersionedTextDocumentIdentifier` is the version after all changes in that notification.
- `TextDocumentContentChangeEvent` is a discriminated union:
  - full-document replacement: `{ text }` — only `text` is present
  - range-based edit: `{ range, text }` (with optional deprecated `rangeLength`)
- If a document `languageId` changes and the server also handles the new language id, model it as `didClose` for the old identity and `didOpen` for the new one.

## Save hooks

- `textDocument/willSave` is a notification.
- `textDocument/willSaveWaitUntil` is a request returning text edits.
- `textDocument/didSave` is a notification.
- When the server has registered for open/close, the client should ensure the document is open before sending `willSave` or `willSaveWaitUntil` (since the client cannot modify file content without ownership transfer).
- Clients may drop `willSaveWaitUntil` results if the server is too slow or constantly fails on this request.
- `didSave` may include text if the server asked for it through `SaveOptions.includeText` (or `TextDocumentSaveRegistrationOptions.includeText`).

## Text document rename signaling

- Model a rename as:
  - `textDocument/didClose` on the old URI
  - `textDocument/didOpen` on the new URI
- Do not treat text-document rename as an independent state model separate from close/open.

## Notebook synchronization

- Notebook sync has two distinct approaches, determined by how the server registers:
  - **Cell-content mode**: The server registers via standard `textDocument/*` selectors. `notebookDocumentSync` is not provided. Only cell text content is synced through standard `textDocument/did*` notifications. Notebook document structure and cell metadata are not synchronized. The server can use a `NotebookCellTextDocumentFilter` to target specific notebook cell documents (e.g., Python cells in Jupyter notebooks).
  - **Notebook mode**: The server provides `notebookDocumentSync` with `notebookSelector` in its capabilities. The full notebook document structure (cells, metadata, execution summaries) is synchronized. Cell text content is bundled with structural changes through `notebookDocument/didChange` notifications and is not synced through standard `textDocument/did*` notifications.
- Synchronize notebooks with (only sent when the server requests `notebook` sync mode):
  - `notebookDocument/didOpen`
  - `notebookDocument/didChange`
  - `notebookDocument/didSave`
  - `notebookDocument/didClose`
- Notebook cell text documents are always updated incrementally (change events carry incremental diffs). In cell-content mode, synchronization goes through standard `textDocument/did*` notifications. In notebook mode, cell text content is synchronized through `notebookDocument/didChange` (via `cells.textContent`), not through `textDocument/did*`. In both modes, cell text documents are treated as regular text documents for standard language features.
- Cell text-document URIs are opaque and must not be parsed by scheme or path assumptions.
- Cell text-document URIs must be unique across all notebook cells.
- `notebookDocument/didChange` bundles changes in a `NotebookDocumentChangeEvent` with three distinct sub-properties:
  - `cells.structure` — structural changes (add/remove/reorder cells via `NotebookCellArrayChange`; also includes `didOpen?: TextDocumentItem[]` and `didClose?: TextDocumentIdentifier[]` to register or deregister affected cell text documents)
  - `cells.data` — cell property changes (kind, execution summary, metadata)
  - `cells.textContent` — cell text content changes
- Additionally, notebook metadata changes are reported via `metadata` at the event level.
- Notebook registration uses `notebookDocumentSync` with `notebookSelector` and optional save support.
- `save` in notebook sync only matters when the sync mode is `notebook`.
- Selector rules:
  - notebook-only filter means sync all cells in matching notebooks
  - cell-only filter means sync all notebooks containing matching cells
- Use notebook and cell filters when standard `textDocument/*` features need to target notebook cell documents.
