# GLuaLS Repository Instructions

## Repository Scope

- This repository is the Rust backend for GLuaLS, a Garry's Mod GLua language server forked from EmmyLua Analyzer Rust.
- Garry's Mod correctness and large-workspace performance take priority. Generic Lua language-server compatibility is out of scope unless a task explicitly requires it.
- The language server used by the VSCode extension is the primary product. `glua_check` and other tools must reuse the same analyzer behavior rather than grow separate rules.
- Editor UI and shipped annotations live in adjacent repositories, usually `vscode-gmod-glua-ls` and `annotations-gmod-glua-ls`. Locate annotations through the adjacent checkout or `BENCH_ANNOTATIONS` when cross-repository validation is needed.
- If expected GLua behavior is unclear, confirm the Garry's Mod semantics before implementing generic Lua behavior.

## Workspace Map

- `crates/glua_code_analysis`: VFS, indexes, analyzer, semantic model, diagnostics, configuration, embedded resources, and most tests.
- `crates/glua_ls`: LSP server and editor-facing handlers. Handlers should consume analyzer APIs and indexes, not reproduce semantic analysis.
- `crates/glua_parser`: parser, AST, and syntax APIs.
- `crates/glua_check`: CLI diagnostics runner and the preferred corpus-diagnostics entry point.
- `crates/glua_doc_cli`, `crates/schema_to_glua`, and `tools/schema_json_gen`: documentation and schema tooling.
- `tools/benchmark`: large-workspace benchmark. It requires `BENCH_CODEBASE` and `BENCH_ANNOTATIONS`.
- `tools/determinism`: diagnostic determinism harness. It requires `DET_CODEBASE` and `DET_ANNOTATIONS`, and answers whether re-analysing a workspace yields the same diagnostics as building it cold. See the module docs for the stage list.
- `docs/mintlify`: user documentation. Follow its nested `AGENTS.md` for changes under that tree.

## Analysis Architecture

- `EmmyLuaAnalysis` in `crates/glua_code_analysis/src/lib.rs` is the top-level owner of workspace state, configuration, VFS, compilation, diagnostics, and incremental updates.
- `glua_code_analysis` is the single source of semantic behavior. The LSP and `glua_check` should consume its indexes and APIs rather than implement their own versions of analysis rules.
- GLuaLS defaults to and assumes `gmod.enabled` is on; disabling it is unsupported. Do not treat Garry's Mod behavior as an optional compatibility layer.
- Extensible Garry's Mod API behavior is annotation-driven. Call roles, wrapper behavior, and guard metadata are shared through signature metadata; check `crates/glua_code_analysis/src/db_index/signature/gmod_domains.rs` before adding a name-based recognizer.
- Realm and load-order analysis are first-class. Consider them when changing semantic or editor behavior, and reuse the shared analyzer/index support rather than adding feature-local heuristics. It is very important for the language server to be realm aware.
- Realm evidence is not path-only: annotations, branches, load edges, filename conventions, and defaults can all contribute. Identically named declarations may legitimately coexist in different realms.
- Analyzer phase ordering should be treated with caution since it can result in severe regressions, always double-check the current order as in the codebase before making changes.
- Cross-file analysis should be indexed or precomputed. Diagnostics already provide shared batch data through `SharedDiagnosticData`; reuse it instead of scanning the workspace per file or request.

## Change Requirements

- Always load rust-best-practice skill, and if working on core language server API functionality, the language server spec skill.
- Fix incorrect inference, realm, load, or member evidence at its root source. Suppressing a diagnostic or adding a special case usually hides the real bug.
- Incremental edits may invalidate dependent files and cross-file caches. Test edit, deletion, and reopen behavior when changing indexes or cached inference.
- Dynamic fields and flow narrowing are sensitive to ownership, source range, scope, realm visibility, and edit stability; preserve all of those dimensions.
- VGUI/scripted classes and helpers such as `AccessorFunc` and `NetworkVar` often use indexed metadata or synthesized members rather than ordinary declarations. Extend the shared model instead of recognizing them separately in each feature.
- Network diagnostics compare send/receive flows and operation order. Treat dynamic message names, payload branches, and read/write loops conservatively to avoid false positives.
- Annotation metadata changes need both ingestion coverage and a downstream behavior test. Use the existing Garry's Mod builtins and fixtures rather than recreating behavior in the test.
- Output derived from hash maps or parallel collection must be sorted before it reaches diagnostics, completions, code lenses, or snapshots.
- Do not address performance problems with arbitrary budgets, caps, fragile pre-filters or broad work-skipping flags. Profile first, then prefilter, index, cache, or parallelize safe read-only work.
- Configuration changes must update the config structs, `crates/glua_code_analysis/resources/schema.json`, generated schema output, and user documentation together. Run `cargo run --bin schema_json_gen` and inspect the resulting diff.
- `.gluarc.json` is exclusive when present; otherwise configs are considered in order: `.luarc.json`, `.emmyrc.json`, `.emmyrc.lua`. Gamemode-base detection scans workspace roots, not the config-file directory.
- Annotations are external library workspaces, not server-bundled files. Loading may come from `glua_check --gmod-annotations`, `glua_ls --gmod-annotations-path`, or the `gmod.annotationsPath` / `gmod.autoLoadAnnotations` settings.

## Testing and Performance

- Use `VirtualWorkspace` and realistic addon or gamemode paths when behavior depends on workspace layout, load order, or realm. Prefer the established Garry's Mod test modules and fixtures over isolated ad hoc cases.
- Call-role and annotation-driven tests should load the relevant builtins; otherwise they may pass while bypassing the real metadata path.
- Typical test commands are `cargo test -p glua_code_analysis <test_name>`, `cargo test -p glua_code_analysis`, and `cargo test`.
- Use `glua_check` JSON output for before/after corpus diagnostic comparisons. The benchmark measures performance; it is not a diagnostics oracle.
- Changes to indexes, cached inference, or the unresolve/resolution passes must keep incremental re-analysis equal to a cold build. Verify with `cargo run --release -p determinism` on a real workspace; `repeat`, `order`, `fresh`, `reindex`, `allreindex` and `mainexpand` are expected to report IDENTICAL. `edit` is also a gate. `mainreindex`, `exact` and `split:N` are bisect stages, not gates: they run `reindex_files_without_expansion`, which skips production's convergence passes, so they are expected to diverge and only matter for localising a failure the gates already caught. Re-run it before and after, because a change can make a stage identical by *degrading* the cold build rather than by fixing the re-index.
- Performance changes require profiling or a targeted before/after benchmark. Use `GLUALS_PROFILE=1` for phase timings and `cargo run --release -p benchmark` for the large-workspace harness.
- Performance is extremely important; the language server must be quick and responsive on large workspaces without loss of functionality. You are to always optimise at the root cause of performance issues. Things such as budgets, string based prefilters / guards and other similar "hacks" are unacceptable since they will regress functionality in large or complex codebases.

## Commands

- Format: `cargo fmt --all`.
- CI-equivalent lint: `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- Pre-commit hygiene: `pre-commit run --all --hook-stage manual`.
- Local release build: `cargo build --release`, optionally with `-p glua_ls`, `-p glua_check`, or `-p glua_doc_cli`.
- Shipped/CI optimized build: `cargo build --profile dist`.
- Docs commands run from `docs/mintlify`: `mint dev` and `mint broken-links`.
