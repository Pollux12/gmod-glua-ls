---
name: language-server-protocol-spec
description: Guide for adherence to Language Server Protocol 3.17 specification. Use when working on a language server or related features such as client/server message flow, capability negotiation, text or notebook synchronization, diagnostics, completions, edits, workspace features, window features, or protocol data structures.
license: MIT
compatibility: LSP 3.17
metadata:
  author: Pollux
  version: "1.0.0"
---

# Language Server Protocol 3.17 Specification Guidelines

Use this skill as a specification-grounded guide for Language Server Protocol 3.17 work. Treat the bundled references as the working index of the 3.17 spec. Based on Microsoft's [Language Server Protocol Specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)

## Workflow

1. Decide which protocol slice the task touches before coding.
2. Read only the matching reference files from `references/`.
3. Implement or review against exact method names, capability paths, registration options, request params, response/result shapes, progress support, and error behavior.
4. Verify that lifecycle, synchronization, and capability negotiation stay internally consistent.
5. If a behavior is ambiguous in the task, prefer the stricter interpretation that matches the spec text and do not mix in behavior from other LSP versions.

## Protocol Rules

- Use JSON-RPC 2.0 over the LSP base protocol header/content framing.
- Require `Content-Length`; default `Content-Type` to `application/vscode-jsonrpc; charset=utf-8`.
- Treat UTF-8 as the supported content encoding; accept `utf8` only as a backwards-compatibility alias.
- Send a response for every processed request, even if the result is `null`; never respond to notifications.
- Keep `initialize` first, `initialized` after a successful initialize response, `shutdown` before `exit`, and do not send extra traffic after shutdown except `exit`.
- Gate behavior on advertised client and server capabilities.
- Only use dynamic registration where the relevant client capability says `dynamicRegistration`.
- Preserve resolve/refresh/two-stage flows exactly as specified for each feature.
- Treat `$/cancelRequest`, `$/progress`, work-done progress, and partial results as part of the contract where a feature declares them.
- Use synchronized document or notebook state as the source of truth for feature computation.

## Reference Map

- Read [references/protocol-and-lifecycle.md](./references/protocol-and-lifecycle.md) for transport framing, JSON-RPC message rules, lifecycle, cancellation, progress, initialization, capability negotiation, dynamic registration, and implementation considerations.
- Read [references/core-types.md](./references/core-types.md) for foundational shared types such as positions, ranges, locations, document selectors, markup content, partial results, work-done progress payloads, regular-expression capabilities, command literals, diagnostic literals, and text-document edit invariants.
- Read [references/shared-types-and-sync.md](./references/shared-types-and-sync.md) for text document synchronization, workspace edits, resource operations, and notebook synchronization.
- Read [references/language-navigation.md](./references/language-navigation.md) for declaration/definition/reference style features, hierarchy features, hover, links, code lens, folding, selection ranges, document symbols, and monikers.
- Read [references/language-editing-and-diagnostics.md](./references/language-editing-and-diagnostics.md) for diagnostics, completion, signature help, semantic tokens, inlay hints, inline values, code actions, formatting, colors, rename, and linked editing.
- Read [references/workspace-window-and-metamodel.md](./references/workspace-window-and-metamodel.md) for workspace requests and notifications, file operations, workspace symbols, window features, and the 3.17 meta model.

## Implementation Checklist

- Match every advertised capability with an implemented handler or an intentional omission.
- Keep request, response, and notification names exact.
- Keep `null` versus omitted values aligned with the spec.
- Preserve versioning and ordering rules for document sync and workspace edits.
- Respect feature-specific resolve semantics: add deferred fields only where the client advertises support and the spec allows it.
- Respect feature-specific refresh semantics: they are global invalidation signals and should be used sparingly.
- Validate edits for overlap and ordering whenever the spec requires non-overlapping text edits.
- Prefer richer result forms only when the client capability allows them.
- Do not invent capability fields, request params, or result members not present in 3.17.

## Source of Truth

The authoritative source is the official LSP 3.17 specification:

- [3.17 specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)
- [3.17 source on GitHub](https://github.com/microsoft/language-server-protocol/tree/gh-pages/_specifications/lsp/3.17)

Use the bundled references first for focus and context efficiency. If a task depends on an exact edge case, verify it against the official 3.17 source instead of guessing.
