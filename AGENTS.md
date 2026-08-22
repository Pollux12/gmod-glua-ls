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
- `tools/lsp_latency.js`: interactive latency harness. It requires `LSP_CODEBASE` and `LSP_ANNOTATIONS`, and drives a real `glua_ls` binary over stdio using the capabilities and cancellation behaviour VS Code actually uses. Reports completion and diagnostic latency settled versus mid-edit, and asserts that a cancelled diagnostic pull never returns an empty full report (which clears a file's diagnostics in VS Code). Use it before and after any change to reindexing or to the freshness gates — those costs are invisible to unit tests.
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
- Changes to indexes, cached inference, or the unresolve/resolution passes must keep incremental re-analysis equal to a cold build. Verify with `cargo run --release -p determinism` on a real workspace; `repeat`, `order`, `fresh`, `reindex`, `allreindex` and `mainexpand` are expected to report IDENTICAL. A third gate, `indexrepeat`, re-indexes each target with its text untouched and requires the **index** to come back identical; the diagnostic gates cannot see index drift, because re-analysis can attach different members or settle a decl's type differently and still produce the same diagnostics. It does **not** pass today (CityRP: 82 type caches, 3 signatures, 11 class members change on a no-op re-index) and that drift is why incremental work cannot be skipped — every "did this actually change?" test answers yes — so treat any *growth* in those counts as yours. It runs last because it re-indexes in place and leaves that warm state behind. The two edit gates are `noopedit` (formerly `edit`) and `realedit`. `noopedit` gates the semantic no-op skip and nothing more: its edit pair is semantically unchanged, so the update path skips the re-index outright and the stage verifies that skipping preserves state — include at least one wide-expansion target (e.g. CityRP `gamemode/core/sh_util.lua`, 1300 files) so the skip is exercised where it matters most. `realedit` is the gate for incremental re-analysis itself, since its edit changes what the file means; set `DET_EDIT_FIND` and `DET_EDIT_REPLACE` or it skips and nothing gates re-analysis. It does **not** pass today, and the divergence is a known gap rather than something you introduced — but it is a small, fixed one, so measure it before and after your change and treat any *growth* as yours. On CityRP, editing `gamemode/core/sh_util.lua` (`function cityrp.util.Bind(self, callback)` gaining a parameter) gives `cold_edited -> warm: removed=0 added=2`, both `need-check-nil` in `plugins/cwweapons` (`cw_base/shared.lua:999`, `cw_m249_official/shared.lua:304`). The cause is visible in the index diff: a signature is identified by its position, an edit moves the positions of every signature after it, and `CallSiteParamIndex` contributions that *target* a moved signature are only dropped when the contributing file is itself re-indexed. A contributor outside the reindex expansion keeps pointing at the old position, so `rebuild_derived_state` splits one parameter's inferred type across the stale signature id and the current one (`15167` keeps one union arm, `15174` the other). Losing an arm widens callers' inferences to include `Unknown`, which is what makes `need-check-nil` fire downstream. Fixing it means invalidating contributions by *target* file and pulling those contributors into the reindex expansion, which changes the expansion set, so measure the benchmark as well when you do. Note it is far more sensitive than the other gates: changes to the unresolve waves, the infer-cache lifetime, or member ownership can break `realedit` while all seven others still report IDENTICAL. `mainreindex`, `exact`, `split:N` and `editmid` are bisect stages, not gates: the first three run `reindex_files_without_expansion`, which skips production's convergence passes, and `editmid` forces a real offset-shifting re-analysis of the expansion (the batch-composition confluence gap), so they are expected to diverge and only matter for localising a failure the gates already caught. Re-run it before and after, because a change can make a stage identical by *degrading* the cold build rather than by fixing the re-index.
- Performance changes require profiling or a targeted before/after benchmark. Use `GLUALS_PROFILE=1` for phase timings and `cargo run --release -p benchmark` for the large-workspace harness.
- For a sampling profile use `samply` (ETW-based on Windows, so it prompts for admin elevation on every run; the user has to approve it). Three things have to be right or you get a useless profile: build with `CARGO_PROFILE_RELEASE_DEBUG=1 cargo build --release -p benchmark` so the PDB exists, run the binary from `target/release` (samply resolves the PDB by the relative path recorded in the exe, so it only finds it from that directory), and do **not** pass `--main-thread-only` — the tools run analysis on a spawned big-stack thread, so the main thread only shows a join. A working invocation is `cd target/release && BENCH_CODEBASE=<path> samply record --save-only --unstable-presymbolicate -o <out>.json.gz ./benchmark.exe`. That writes `<out>.json.gz` plus a `<out>.json.syms.json` sidecar; the profile itself holds only addresses, so symbol names come from joining the two by `libs[].debugName` and the frame address against each module's `symbol_table` rva ranges.
- Performance is extremely important; the language server must be quick and responsive on large workspaces without loss of functionality. You are to always optimise at the root cause of performance issues. Things such as budgets, string based prefilters / guards and other similar "hacks" are unacceptable since they will regress functionality in large or complex codebases.

## Commands

- Format: `cargo fmt --all`.
- CI-equivalent lint: `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- Pre-commit hygiene: `pre-commit run --all --hook-stage manual`.
- Local release build: `cargo build --release`, optionally with `-p glua_ls`, `-p glua_check`, or `-p glua_doc_cli`.
- Shipped/CI optimized build: `cargo build --profile dist`.
- Docs commands run from `docs/mintlify`: `mint dev` and `mint broken-links`.
