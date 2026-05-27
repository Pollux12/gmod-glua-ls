# Core Types

## Contents

- Text document coordinate model
- Core location and selector types
- Markup and regular expressions
- Command and diagnostic literals
- Partial results and work-done progress
- Text edit invariants

## Text document coordinate model

- LSP is defined for textual documents, not binary documents.
- Positions are zero-based line and zero-based character offsets.
- A `Position` is between characters; special values like `-1` are not supported. Both `line` and `character` are `uinteger` (non-negative integers).
- If a position character offset is greater than the line length, it falls back to the line length.
- Character offsets are interpreted using the negotiated `PositionEncodingKind`. The client announces supported encodings (ordered by decreasing preference) via `general.positionEncodings`; the server picks one and signals it back via `capabilities.positionEncoding`. If the client omits `utf-16` from `general.positionEncodings`, servers may still assume UTF-16 support. If the server omits `capabilities.positionEncoding`, it defaults to `utf-16`.
- Predefined 3.17 encodings:
  - `utf-8`
  - `utf-16`
  - `utf-32`
- UTF-16 is the default and must always be supported by servers.
- A `Range` has `start` and `end` positions, and the end position is exclusive.
- Valid line ending sequences are `\n`, `\r\n`, and `\r`, ensuring both client and server split text into the same line representation.
- Positions are line-ending agnostic; you cannot specify a position that denotes `\r|\n` or `\n|` where `|` represents the character offset.

## Core location and selector types

- `Location` is `{ uri, range }`.
- `LocationLink` carries:
  - optional `originSelectionRange`
  - `targetUri`
  - `targetRange`
  - `targetSelectionRange`
- `targetSelectionRange` must be contained by `targetRange`.
- `TextDocumentPositionParams` bundles:
  - `textDocument: TextDocumentIdentifier`
  - `position: Position`
- `DocumentFilter` may match on:
  - `language`
  - `scheme`
  - `pattern`
- At least one `DocumentFilter` property must be set for a valid filter.
- A `DocumentSelector` is an array of one or more `DocumentFilter`.
- Glob patterns: `*` matches zero or more characters in a single path segment; `?` matches one character; `**` matches any number of path segments including none; `{}` groups sub patterns into OR; `[]` declares a character range; `[!...]` negates a range.

## Markup and regular expressions

- `MarkupContent` supports `plaintext` and `markdown`.
- Markup kinds must not start with `$`; such kinds are reserved for internal usage.
- Markdown content should follow GitHub Flavored Markdown.
- Clients may sanitize returned markdown.
- Clients can advertise markdown parser details through `general.markdown` (since 3.16.0), including parser name and optional version. The `allowedTags` field was added in 3.17.0.
- Regular-expression support is described through `general.regularExpressions`.
- The client announces its regex engine name and optional version.
- The LSP regex subset is based on ECMAScript 2020. Features not mandatory for clients: lookahead/lookbehind assertions, caret notation (`\cX`), UTF-16 code unit matching (`\uhhhh`), named capturing groups, and all Unicode property escapes.
- The only regex flag clients are required to support is `i`.

## Command and diagnostic literals

- `Command` contains:
  - `title`
  - `command`
  - optional `arguments` (`LSPAny[]`)
- The recommended model is server-side command execution when the client and server support it.
- `Diagnostic` is valid only in the scope of a resource.
- A diagnostic includes at minimum:
  - `range`
  - `message`
- Servers are highly recommended to always provide a severity.
- If severity is omitted, it is recommended that the client interpret it as `Error`.
- Severity values:
  - `Error = 1`
  - `Warning = 2`
  - `Information = 3`
  - `Hint = 4`
- Diagnostic tags:
  - `Unnecessary = 1`
  - `Deprecated = 2`
- `relatedInformation` carries related `Location` plus message pairs.
- `codeDescription.href` links to more information about a diagnostic code.
- `Diagnostic.data` is preserved between `publishDiagnostics` and `codeAction`.

## Partial results and work-done progress

- `PartialResultParams` adds an optional `partialResultToken`.
- Partial results are reported via `$/progress`.
- If a server streams partial results, the whole result must be reported through n `$/progress` notifications, where each notification appends items to the accumulated result; the final response must be empty in terms of result values.
- If the response error code equals `RequestCancelled`, the client may still use streamed partial results but should make clear that the request was cancelled and results may be incomplete.
- For other errors, streamed partial results should not be used.
- `WorkDoneProgressParams` adds an optional `workDoneToken`.
- Work-done progress payloads are:
  - `begin`
  - `report`
  - `end`
- `begin` requires a `title` and may include `cancellable` (controls whether a cancel button is shown to the user), `message`, and `percentage`.
- `report` may include `cancellable` (controls the enablement state of the cancel button; only valid if a cancel button was requested in `begin`), `message`, and `percentage`.
- `end` may include a final `message`.
- Progress can be initiated:
  - by the client through a per-request `workDoneToken`
  - by the server through `window/workDoneProgress/create` (only when the client signals support via the `window.workDoneProgress` client capability)
- A server needs to signal general work-done progress support in the relevant server capability before clients expect it.
- A server-initiated progress token should be used once for one begin, zero or more report notifications, and one end notification.

## Text edit invariants

- `TextEdit[]` and `AnnotatedTextEdit[]` describe a single logical document change computed against one original state.
- Text edit ranges must never overlap.
- Multiple edits may share the same start position.
- Valid same-start-position patterns: multiple inserts, or any number of inserts followed by a single remove or replace edit. Array order defines insertion order for same-position inserts.
- `TextDocumentEdit` moves one document from version `Si` to `Si+1`. The document is identified by an `OptionalVersionedTextDocumentIdentifier` (when sent from server to client, the server may send `null` for the version when the file has not been opened — i.e. the server has not received an open notification — to signal that the version is known and the on-disk content is the master; a non-null version enables client-side version checking).
- The creator of a `TextDocumentEdit` does not need to sort the edits.
- The edits inside a `TextDocumentEdit` still must be non-overlapping.
- `AnnotatedTextEdit` use is guarded by `workspace.workspaceEdit.changeAnnotationSupport`.
