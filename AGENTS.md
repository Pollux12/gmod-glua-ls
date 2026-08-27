# GLuaLS Repository Instructions

## Repository Scope

- Rust backend for GLuaLS, forked from EmmyLua Analyzer Rust. Garry's Mod correctness and large-workspace performance are primary; generic Lua compatibility is out of scope unless explicitly required.
- The VSCode language server is the product. `glua_check` and other tools must reuse the same analyzer behavior.
- Editor UI and shipped annotations live in adjacent repos (`vscode-gmod-glua-ls`, `annotations-gmod-glua-ls`). Use the adjacent checkout or `BENCH_ANNOTATIONS` env var.
- If GLua semantics are unclear, confirm Garry's Mod behavior before implementing generic Lua.

## Workspace Map

- `crates/glua_code_analysis`: VFS, indexes, analyzer, semantic model, diagnostics, config, embedded resources, most tests.
- `crates/glua_ls`: LSP server and handlers. Consume analyzer APIs; do not reimplement analysis.
- `crates/glua_parser`, `crates/glua_parser_desc`: parser, AST, syntax APIs.
- `crates/glua_check`: CLI diagnostics runner; preferred corpus entry point.
- `crates/glua_doc_cli`, `crates/schema_to_glua`, `tools/schema_json_gen`: docs and schema tooling.
- `tools/benchmark`: large-workspace benchmark (`BENCH_CODEBASE` + `BENCH_ANNOTATIONS` required).
- `tools/determinism`: determinism harness (`DET_CODEBASE` + `DET_ANNOTATIONS` required).
- `tools/lsp_latency.js`: latency harness (`LSP_CODEBASE` + `LSP_ANNOTATIONS`); drives `glua_ls` over stdio with VS Code capabilities. Reports settled vs mid-edit latency and asserts cancelled diagnostic pulls never return empty reports. Run before/after reindexing or freshness-gate changes.
- `docs/mintlify`: user documentation (see nested `AGENTS.md`).

## Analysis Architecture

- `EmmyLuaAnalysis` in `crates/glua_code_analysis/src/lib.rs` owns workspace state, VFS, compilation, diagnostics, and incremental updates.
- `glua_code_analysis` is the single source of semantic behavior.
- `gmod.enabled` defaults on; disabling is unsupported.
- GMod API extensibility is annotation-driven via signature metadata; check `crates/glua_code_analysis/src/db_index/signature/gmod_domains.rs` before adding name-based recognizers.
- Realm and load-order are first-class; reuse shared analyzer/index support. Realm evidence includes annotations, branches, load edges, filenames, and defaults — same name may coexist across realms.
- Analyzer phase ordering is fragile; verify current order before changing it.
- Cross-file work must be indexed. Reuse `SharedDiagnosticData` for diagnostics instead of per-file workspace scans.

## Change Requirements

- Load `rust-best-practices` skill first; also `language-server-spec` for LSP work.
- Fix inference/realm/load/member root cause; do not suppress diagnostics or add special cases.
- Incremental edits may invalidate dependents and caches; test edit, delete, and reopen when changing indexes or cached inference. Preserve ownership, range, scope, realm, and edit stability for dynamic fields/flow narrowing.
- VGUI/scripted classes (`AccessorFunc`, `NetworkVar`, etc.) use indexed/synthesized members; extend the shared model, don't duplicate per-feature.
- Network diagnostics compare send/receive flows and order; be conservative with dynamic names, branches, and loops.
- Annotation metadata changes need ingestion coverage plus a downstream behavior test via real builtins/fixtures.
- Sort any output derived from hash maps or parallel collection before diagnostics/completions/snapshots.
- No budgets, caps, or fragile prefilters for performance. Profile first, then index/cache/optimize/parallelize.
- Config changes must update structs, `crates/glua_code_analysis/resources/schema.json`, and docs together. Run `cargo run --bin schema_json_gen` and commit the diff.
- `.gluarc.json` is exclusive when present; otherwise consider `.luarc.json`, `.emmyrc.json`, `.emmyrc.lua` in order. Gamemode-base detection scans workspace roots.
- Annotations are external library workspaces: `glua_check --gmod-annotations`, `glua_ls --gmod-annotations-path` (or `gmod.annotationsPath` / `gmod.autoLoadAnnotations` in config).

## Testing and Performance

- Use `VirtualWorkspace` with realistic addon/gamemode paths; prefer existing GMod fixtures. Call-role tests must load relevant builtins.
- Tests: `cargo test -p glua_code_analysis <filter>` | `cargo test -p glua_code_analysis` | `cargo test`.
- Corpus diffs: `glua_check` JSON. Benchmark is for performance only.
- Determinism (required for index/cache/unresolve changes): `cargo run --release -p determinism`. Requires `DET_CODEBASE` and `DET_ANNOTATIONS`; set `DET_EDIT_FIND`/`DET_EDIT_REPLACE` for edit gates or they skip. Every gate must be `+0` diagnostics and `+0` index.
  Gates: `repeat`, `fresh`, `order`, `reindex`, `allreindex`, `mainexpand`, `noopedit`, `realedit`, `editrevert`, `indexrepeat`, `burst`.
  Bisect/debug only (expected to diverge): `mainreindex`, `exact`, `split:N`, `editmid`, `restabilize`, `perfile`, `expandwhy`, `faithful`.
  Use `DET_TARGETS=gamemode/core/sh_data.lua` by default for edit target, `sh_configuration` is good for performance related tests (many related files).
- Perf: `GLUALS_PROFILE=1` for phase timings; `cargo run --release -p benchmark` for large-workspace. For `samply` (ETW on Windows, needs elevation and therefore user permission first): build with `CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release -p benchmark`, run from `target/release`, do not use `--main-thread-only` (analysis runs on spawned thread). Example: `cd target/release && BENCH_CODEBASE=<path> samply record --save-only --unstable-presymbolicate -o out.json.gz ./benchmark.exe`.

## Commands

- `cargo fmt --all`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `pre-commit run --all-files` (mixed-line-ending hook is `manual` stage)
- `cargo build --release` [`-p glua_ls|glua_check|glua_doc_cli`]
- `cargo build --profile dist` (shipped/CI optimized, thin LTO)
- `docs/mintlify`: `mint dev` | `mint broken-links`
