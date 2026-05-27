# Language Navigation

## Contents

- Navigation requests
- Hierarchy requests
- Comprehension features
- Shared result-shape rules
- Feature-specific edge cases

## Navigation requests

- `textDocument/declaration`
- `textDocument/definition`
- `textDocument/typeDefinition`
- `textDocument/implementation`
- `textDocument/references`
  - Requires a `context.includeDeclaration` boolean that controls whether the symbol's own declaration is included in results.

Respect the corresponding client capability paths and server capability providers:

- `textDocument.declaration` / `declarationProvider`
- `textDocument.definition` / `definitionProvider`
- `textDocument.typeDefinition` / `typeDefinitionProvider`
- `textDocument.implementation` / `implementationProvider`
- `textDocument.references` / `referencesProvider`

For declaration, definition, type definition, and implementation, `linkSupport` only gates the `LocationLink[]` result form. Without link support, stay within the spec's allowed non-link forms such as `Location`, `Location[]`, or `null`, depending on the feature result contract. `textDocument/references` has no `linkSupport` capability and always returns `Location[] | null` only.

## Hierarchy requests

- Call hierarchy:
  - `textDocument/prepareCallHierarchy`
  - `callHierarchy/incomingCalls`
  - `callHierarchy/outgoingCalls`
- Type hierarchy:
  - `textDocument/prepareTypeHierarchy`
  - `typeHierarchy/supertypes`
  - `typeHierarchy/subtypes`

Rules:

- The follow-up hierarchy requests are only issued if the server registered for the corresponding prepare request.
- Preserve server-provided `data` across the prepare and follow-up stages.
- `selectionRange` must be contained by the item `range`.
- Prepare requests may return `null` when the server cannot infer a valid hierarchy item.

## Comprehension features

- `textDocument/documentHighlight`
- `textDocument/documentLink`
- `documentLink/resolve`
- `textDocument/hover`
- `textDocument/codeLens`
- `codeLens/resolve`
- `workspace/codeLens/refresh`
- `textDocument/foldingRange`
- `textDocument/selectionRange`
- `textDocument/documentSymbol`
- `textDocument/moniker`

Respect the corresponding capability paths and provider fields:

- `textDocument.documentHighlight` / `documentHighlightProvider`
- `textDocument.documentLink` / `documentLinkProvider` (type: `DocumentLinkOptions` only — not a plain `boolean`) with optional `resolveProvider`
- `textDocument.hover` / `hoverProvider`
- `textDocument.codeLens` / `codeLensProvider` (type: `CodeLensOptions` only — not a plain `boolean`) with optional `resolveProvider`
- `workspace.codeLens.refreshSupport` / `workspace/codeLens/refresh`
- `textDocument.foldingRange` / `foldingRangeProvider`
- `textDocument.selectionRange` / `selectionRangeProvider`
- `textDocument.documentSymbol` / `documentSymbolProvider`
- `textDocument.moniker` / `monikerProvider`

## Shared result-shape rules

- Prefer `MarkupContent` over deprecated `MarkedString` where hover-style markup is supported.
- Preserve `data` for deferred resolve flows like document link resolve and code lens resolve.
- Support partial results where the feature declares them.
- For `documentSymbol`, do not mix `DocumentSymbol[]` and `SymbolInformation[]` within one result stream. The first chunk fixes the stream type.
- Prefer hierarchical `DocumentSymbol` results when supported. `SymbolInformation` does not reconstruct hierarchy.

## Feature-specific edge cases

- Hover positions are typically sent immediately to the left of the hovered character, but the exact interpretation is language-dependent.
- `DocumentHighlight` is intentionally less strict than references and uses kinds such as `Text`, `Read`, and `Write`.
- `DocumentLink.tooltip` can include trigger instructions that vary by OS, settings, and localization.
- Code lens ranges should only span a single line.
- `workspace/codeLens/refresh` is a global invalidation signal and should be used sparingly.
- `FoldingRangeClientCapabilities.rangeLimit` is only a hint.
- If the client is line-folding only, ignore character offsets in folding ranges.
- Unknown folding kinds must be handled gracefully.
- Clients may ignore invalid folding ranges.
- `SelectionRange` results are returned position-by-position; `positions[i]` must be contained in `result[i].range`. The empty range located at the queried position (`positions[i]`) is allowed as a fallback when no better selection exists. Each `SelectionRange.parent.range` must contain `this.range`.
- `DocumentSymbol.name` must not be empty or whitespace-only.
- `DocumentSymbol.selectionRange` must be contained by `DocumentSymbol.range`.
- Moniker requests should return `null` or an empty array if no monikers can be computed.
- The moniker calculation should stay aligned with LSIF moniker semantics as noted by the spec.
