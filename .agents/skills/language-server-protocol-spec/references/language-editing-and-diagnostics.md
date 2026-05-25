# Language Editing And Diagnostics

## Contents

- Diagnostics
- Completion and signature help
- Semantic tokens, inlay hints, inline values
- Code actions
- Formatting and colors
- Rename and linked editing

## Diagnostics

### Push diagnostics

- Use `textDocument/publishDiagnostics`.
- Diagnostics are server-owned.
- When there are no diagnostics, publish an empty array to clear prior diagnostics.
- Each publish replaces the previous published diagnostics for that document; the client does not merge them.
- `PublishDiagnosticsParams.version` is optional and only matters when the client supports version interpretation.
- For single-file languages, diagnostics are typically cleared on close.
- For project systems, close does not necessarily clear diagnostics.

### Pull diagnostics

- Use:
  - `textDocument/diagnostic`
  - `workspace/diagnostic`
  - `workspace/diagnostic/refresh`
- The server must compute document diagnostics against the currently synchronized document version.
- If a document report returns `unchanged`, it must only do so when a `previousResultId` was provided in the request, and must include a `resultId` to be used in the next diagnostic request for the same document.
- Workspace diagnostic streams may emit multiple reports for the same URI; the last report wins.
- When both document-pull and workspace-pull diagnostics exist for the same URI, diagnostics for a higher document version should win over a lower version, and document-pull diagnostics should take precedence over workspace-pull diagnostics for the same document version.
- If a pull diagnostic request returns `ServerCancelled` (-32802) without a `DiagnosticServerCancellationData` payload, the default client behavior is to retrigger it (`{ retriggerRequest: true }`).
- Pull diagnostics may include related documents and partial result streaming where declared.

## Completion and signature help

### Completion

- `textDocument/completion`
- `completionItem/resolve`
- Capability path: `textDocument.completion`
- Provider: `completionProvider`

Rules:

- Returning `CompletionItem[]` means the list is complete, equivalent to `CompletionList` with `isIncomplete: false`.
- Use lazy resolution only for properties the client advertised as resolvable.
- A resolve response should fill missing fields but must not change existing completion item attributes.
- Respect trigger kinds, including retriggering for incomplete lists.
- `itemDefaults` can only be used when the client advertises support via `completionList.itemDefaults` (a `string[]` of supported property names); the server may only apply defaults for the specific properties listed.
- Completion items with `textEdit` change client filtering and word-guess behavior compared with plain insert text.

### Signature help

- `textDocument/signatureHelp`
- Capability path: `textDocument.signatureHelp`
- Provider: `signatureHelpProvider`

Rules:

- Respect trigger and retrigger behavior through signature help context when supported.
- If `activeSignature` is omitted or out of range, it defaults to 0, or is ignored when `SignatureHelp` has no signatures at all. If `activeParameter` is omitted or out of range, it defaults to 0 when the active signature has parameters, and is ignored when the active signature has no parameters. Since 3.16.0, prefer `SignatureInformation.activeParameter` over `SignatureHelp.activeParameter`.

## Semantic tokens, inlay hints, inline values

### Semantic tokens

- Methods:
  - `textDocument/semanticTokens/full`
  - `textDocument/semanticTokens/full/delta`
  - `textDocument/semanticTokens/range`
  - `workspace/semanticTokens/refresh`
- Capability path: `textDocument.semanticTokens`
- Provider: `semanticTokensProvider`

Rules:

- The server legend must define every token type and modifier the server emits.
- Delta results must not be assumed sorted by the client. An effective approach is to sort the edits and apply them back-to-front.
- If a range request is answered with a broader range, the broader result must still be complete and correct.
- The server should include tokens that only partially overlap with the requested range boundaries.
- Multi-line and overlapping tokens require corresponding client support.
- Token type numeric values are expected to stay below `65536`.
- Refresh is global and should be used carefully.

### Inlay hints

- Methods:
  - `textDocument/inlayHint`
  - `inlayHint/resolve`
  - `workspace/inlayHint/refresh`
- Capability path: `textDocument.inlayHint`
- Provider: `inlayHintProvider`

Rules:

- Hint labels and label parts must not be empty.
- Resolve only deferred fields allowed by the spec and client capabilities.
- Label parts with `location` become clickable links that resolve to the definition of the symbol at the given location (not necessarily the location itself); the editor will show a hover for that location and a context menu with further code navigation commands.
- Padding uses the editor background color semantics described by the spec.
- Refresh is global.

### Inline values

- Methods:
  - `textDocument/inlineValue`
  - `workspace/inlineValue/refresh`
- Capability path: `textDocument.inlineValue`
- Provider: `inlineValueProvider`

Rules:

- Compute inline values for the viewport document range provided in `InlineValueParams.range`. The request always includes a required `InlineValueContext` with `frameId` (the DAP stack frame ID) and `stoppedLocation` (a separate range where execution stopped, used as context for computing values).
- Use refresh globally and sparingly.

## Code actions

- Methods:
  - `textDocument/codeAction`
  - `codeAction/resolve`
- Capability path: `textDocument.codeAction`
- Provider: `codeActionProvider`

Rules:

- A code action must provide a `title` plus `edit` and/or `command`.
- If both exist, apply the edit first and then execute the command.
- Resolve should add missing properties only and should not mutate existing ones.
- Code action kinds are open-ended hierarchical strings.
- `CodeActionContext.triggerKind` indicates whether the action was explicitly invoked (`Invoked = 1`) or automatically triggered (`Automatic = 2`).
- `source.fixAll` automatically fixes errors that have a clear fix not requiring user input; it should not suppress errors or perform unsafe fixes such as generating new types or classes.
- Disabled code actions are hidden from lightbulb menus and shown faded in more targeted menus.

## Formatting and colors

### Formatting

- Methods:
  - `textDocument/formatting`
  - `textDocument/rangeFormatting`
  - `textDocument/onTypeFormatting`
- Capability paths:
  - `textDocument.formatting`
  - `textDocument.rangeFormatting`
  - `textDocument.onTypeFormatting`
- Providers:
  - `documentFormattingProvider`
  - `documentRangeFormattingProvider`
  - `documentOnTypeFormattingProvider`

Rules:

- `FormattingOptions` includes standard fields plus free-form keyed properties.
- For on-type formatting, `position` is not guaranteed to be the exact character position of the typed trigger.
- For on-type formatting, `ch` may not be the last inserted character because of client auto-insert behavior.

### Colors

- Methods:
  - `textDocument/documentColor`
  - `textDocument/colorPresentation`
- Capability path: `textDocument.colorProvider`
- Provider: `colorProvider`

Rules:

- `additionalTextEdits` in color presentation must not overlap with the main edit or with each other.

## Rename and linked editing

### Rename

- Methods:
  - `textDocument/rename`
  - `textDocument/prepareRename`
- Capability path: `textDocument.rename`
- Provider: `renameProvider`

Rules:

- Invalid `newName` must return a `ResponseError` with a message.
- `prepareRename` returning `null` means rename is invalid at that position.
- `prepareRename` may return one of:
  - `Range` — the range of the symbol to rename
  - `{ range: Range, placeholder: string }` — the rename range plus a suggested placeholder string
  - `{ defaultBehavior: boolean }` — the position is valid; client should use its default behavior to compute the rename range
- `RenameOptions` may only be used when the client supports prepare support.
- A `null` rename result means no change is required.

### Linked editing

- Method: `textDocument/linkedEditingRange`
- Capability path: `textDocument.linkedEditingRange`
- Provider: `linkedEditingRangeProvider`

Rules:

- All returned ranges must have identical length and identical text content.
- Returned ranges must not overlap.
- If the server does not provide a word pattern, the client uses its own.
