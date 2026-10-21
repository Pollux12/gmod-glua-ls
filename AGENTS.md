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
- `tools/lsp_latency.js`: latency harness (`LSP_CODEBASE` + `LSP_ANNOTATIONS`); drives `glua_ls` over stdio with VS Code capabilities. Reports settled vs mid-edit latency and fails on mid-edit completion drift; its diagnostic-pull checks apply only to pull servers, so `glua_ls` (push) diagnostics are gated by `tools/lsp_drift.js`. Run before/after reindexing or freshness-gate changes.
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
- No budgets, caps, or fragile prefilters for performance: they regress functionality on exactly the large or complex workspaces the server exists for. Profile first, then index/cache/optimize/parallelize. Fix performance at the root cause.
- Config changes must update structs, `crates/glua_code_analysis/resources/schema.json`, and docs together. Run `cargo run --bin schema_json_gen` and commit the diff.
- `.gluarc.json` is exclusive when present; otherwise consider `.luarc.json`, `.emmyrc.json`, `.emmyrc.lua` in order. Gamemode-base detection scans workspace roots.
- Annotations are external library workspaces: `glua_check --gmod-annotations`, `glua_ls --gmod-annotations-path` (or `gmod.annotationsPath` / `gmod.autoLoadAnnotations` in config).

## Testing and Performance

- Use `VirtualWorkspace` with realistic addon/gamemode paths; prefer existing GMod fixtures. Call-role tests must load relevant builtins.
- Tests: `cargo test -p glua_code_analysis <filter>` | `cargo test -p glua_code_analysis` | `cargo test`.
- Validation tiers: `tools/validate.sh change|step|phase` (env `GLUALS_ANNOTATIONS`, `GLUALS_VB`, `GLUALS_CITYRP`, `GLUALS_GAMEMODES`; `VALIDATE_REF` holds `vb.json`/`cityrp.json` references; `CHECK_CACHE=1` adds cache check mode for cache/TSC changes). While coding run targeted tests only; run `change` (vehicle base, ~2-3 min) before each commit that touches analysis, `step` (CityRP, ~6-8 min) at the end of a task, `phase` at a phase exit. Don't run CityRP or sweep stages per change.
- Corpus diffs: `glua_check` JSON (`node tools/diagnostics_diff.js ref.json new.json`). Benchmark is for performance only.
- Determinism (required for index/cache/unresolve changes): `cargo run --release -p determinism`. The harness module docs say what each stage proves; read them before interpreting a result. Requires `DET_CODEBASE` and `DET_ANNOTATIONS`; targets and edits come from `DET_EDITS` (per-target edits) or `DET_TARGETS` with `DET_EDIT_FIND`/`DET_EDIT_REPLACE`. `DET_JOBS` caps analyses alive at once (2 for CityRP on a 16 GB machine). **Every stage must be `+0` diagnostics and `+0` index: no stage is expected to diverge, so a non-zero result is a defect to fix, never an accepted outcome.**
  Gates: `edit`, `revert`, `noop`, `reload`, `schedule`, `split:2`, and the sweeps `touch`, `shift`, `delete`, `recreate`, `rename` (batched; `DET_EACH=1` for one file per run).
  Bisect aids (same `+0` requirement): `typing` (keystrokes mid re-analysis, index-level), `exact`, `burst`, `schedule:SEED`, `split:N`, `DET_EACH=1`, `DET_REPLAY=1`.
  The run exits non-zero on any divergence, including a `noop`/`touch`/`shift` re-analysis that evaluated files beyond the edited ones, so a green exit means every stage that ran passed.
- Server drift gate: `node tools/lsp_drift.js` drives a real `glua_ls` (typing with real debounce, revert, watched delete and re-create) and compares every diagnostic it shows with a cold `glua_check`; part of every tier.
- Perf: `GLUALS_PROFILE=1` for phase timings; `cargo run --release -p benchmark` for large-workspace. For `samply` (ETW on Windows, needs elevation and therefore user permission first) three things have to be right or the profile is useless: build with `CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release -p benchmark` so the PDB exists; run from `target/release`, because samply resolves the PDB by the relative path recorded in the exe; and do not pass `--main-thread-only`, because analysis runs on a spawned big-stack thread and the main thread only shows a join. Example: `cd target/release && BENCH_CODEBASE=<path> samply record --save-only --unstable-presymbolicate -o out.json.gz ./benchmark.exe`. That writes `out.json.gz` plus an `out.json.syms.json` sidecar; the profile holds only addresses, so symbol names come from joining the two by `libs[].debugName` and the frame address against each module's `symbol_table` rva ranges.
- Before running Samply on Windows, check `tools/samply-bridge.ps1 -Status` and use that bridge when available.

## Commands

- `cargo fmt --all`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `pre-commit run --all-files`, and `pre-commit run --all --hook-stage manual` to include the manual-stage hooks (mixed-line-ending)
- `cargo build --release` [`-p glua_ls|glua_check|glua_doc_cli`]
- `cargo build --profile dist` (shipped/CI optimized, thin LTO)
- `docs/mintlify`: `mint dev` | `mint broken-links`
