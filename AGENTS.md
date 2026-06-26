# Agent Notes

## Project Purpose
- This repository is the Rust backend for GLuaLS, a Garry's Mod GLua language server hard-forked from EmmyLua Analyzer Rust.
- Treat Garry's Mod Lua as the default language and runtime context. Avoid qualifying core behavior as if it were an optional specialization unless the contrast adds useful information.
- Optimize for Garry's Mod correctness and large-workspace performance first. Generic Lua language-server compatibility is not a goal unless a task explicitly asks for it.
- The primary product is the language server used by the VSCode extension. `glua_check` and other tools should share the same analyzer behavior instead of growing separate rules.
- The VSCode extension supplies editor UI, settings UX, automatic annotation downloads, and some client integration. Server changes that need shipped annotations or editor UI may also need adjacent repos, commonly `vscode-gmod-glua-ls` and `annotations-gmod-glua-ls` near this workspace or via `BENCH_ANNOTATIONS`.

## Workspace Map
- `crates/glua_code_analysis`: static-analysis engine, VFS, indexes, semantic model, diagnostics, config, embedded std resources, and most tests.
- `crates/glua_ls`: LSP server and editor-facing handlers. Prefer consuming analyzer indexes here rather than duplicating analyzer logic in handlers.
- `crates/glua_check`: CLI diagnostics runner. Good for corpus diagnostic comparison because it exercises real workspace loading and shared diagnostic precompute.
- `crates/glua_parser`: parser and AST/syntax APIs.
- `crates/glua_doc_cli`: documentation generation tooling.
- `crates/schema_to_glua` and `tools/schema_json_gen`: config schema generation and conversion.
- `tools/benchmark`: release-mode large-workspace benchmark; requires `BENCH_CODEBASE` and `BENCH_ANNOTATIONS`.
- `docs/mintlify`: user docs site. Its own agent notes are in `docs/mintlify/AGENTS.md`.

## Core Analysis Model
- Treat indexing as the source of shared truth. Later analysis, diagnostics, semantic queries, completions, hovers, code lenses, and LSP handlers should consume cached/indexed structures instead of rescanning the workspace.
- Prefer shared APIs and shared indexed facts over repeated local logic. The point of systems like the realm model, load model, call-role metadata, and shared diagnostics is to compute expensive or subtle facts once, expose them through stable APIs, and reuse them everywhere.
- `EmmyLuaAnalysis` in `crates/glua_code_analysis/src/lib.rs` owns the VFS, `LuaCompilation`, diagnostics, config, workspace roots, incremental file updates, and cross-file cache stabilization.
- The main analysis pipeline order in `crates/glua_code_analysis/src/compilation/analyzer/mod.rs` is intentional:
  1. `DeclAnalysisPipeline`
  2. `DocAnalysisPipeline`
  3. `GmodPreAnalysisPipeline`
  4. `FlowAnalysisPipeline`
  5. `LuaAnalysisPipeline`
  6. `GmodPostAnalysisPipeline`
  7. `synthesize_accessorfunc_members`
  8. `EarlyDynamicFieldAnalysisPipeline` when dynamic-field inference is enabled
  9. `UnResolveAnalysisPipeline`
  10. `synthesize_setmetatable_factory_members`
  11. `CallSiteParamAnalysisPipeline`
  12. `DynamicFieldAnalysisPipeline` when dynamic-field inference is enabled
  13. `UnResolveAnalysisPipeline` again when dynamic-field inference is enabled
  14. `synthesize_setmetatable_factory_members` again when dynamic-field inference is enabled
  15. `resolve_uninformative_local_decl_caches` when dynamic-field inference is enabled
- Do not move realm/load/hook/network work between phases casually. That metadata must exist before flow and Lua analysis so caches are keyed with the right realm from the start. Some dynamic outparam fields must be seeded before the first unresolve pass, while full dynamic-field collection still depends on unresolve-refined aliases and therefore triggers a second unresolve/stabilization pass.
- Setmetatable-factory synthesis is also part of the real pipeline now, not an incidental helper. If a change affects unresolve-driven member discovery, account for the post-unresolve `setmetatable` synthesis passes as part of the analyzer contract.
- Incremental edits are not always single-file. `expand_reindex_file_ids` and cross-file type-cache stabilization protect dependent files from stale cached types after edits.
- Loading order and realm inference are shared indexed systems, not separate ad hoc heuristics. `GmodLoadIndex` owns engine/addon load roots, load edges, state masks, confidence, and file load status; `GmodInferIndex` then stores per-file realm metadata derived from annotations, branch ranges, load info, filename hints, and config defaults. Prefer extending those shared indexes over re-deriving load order or realm in downstream features.
- Before adding new analyzer, diagnostic, or handler logic, check whether the needed fact should come from an existing shared API or index first. Adding a second way to answer the same question is usually a design bug here.

## Language And Annotation Model
- `gmod.enabled` is the default mode, not an optional side path. Any feature that affects symbols, type inference, diagnostics, completions, code lenses, definitions, references, or hovers must be checked for realm-aware behavior.
- The repository is now intentionally annotation-driven for extensible Garry's Mod behavior. Treat annotations and indexed metadata as the primary control plane for wrappers, custom helpers, and addon-specific extensions before considering any hardcoded recognizer.
- Realm awareness is first-class. Relevant data lives in `GmodInferIndex`, `GmodLoadIndex`, `GmodStateMask`, and related helpers under `crates/glua_code_analysis/src/db_index/gmod_*` plus `compilation/analyzer/gmod/mod.rs`.
- The loading-order model is also first-class and shared. Engine roots such as `autorun`, `vgui`, `skins`, `effects`, `stools`, gamemode entrypoints, scripted classes, and annotated load wrappers are modeled centrally and then consumed by realm inference and diagnostics.
- Realm evidence can come from explicit `---@realm`, branch ranges such as `if CLIENT then`, file names, Garry's Mod path layout, engine-loaded roots, include/AddCSLuaFile/IncludeCS/require load edges, annotated wrapper functions, and config defaults. Do not replace this with path-only inference.
- `Shared` means client and server runtime states. `Menu` has special caller compatibility with client in `GmodStateMask`; do not collapse it into generic client behavior without checking tests.
- Call roles and wrapper metadata use attributes such as `---@attribute call_arg`, `call_arg_field`, `overload_call_arg`, and `overload_call_arg_field`. The active domains and reserved metadata names are centralized in `crates/glua_code_analysis/src/db_index/signature/gmod_domains.rs`; extend those shared consumers instead of scattering literal strings or per-handler special cases.
- Guard and narrowing behavior is also moving through metadata. In addition to `gmod.member_guard` call roles, signature-level attributes such as `self_guard` and `valid_guard` are part of the intended extension path for recognizers that should generalize beyond one hardcoded function name.
- Assume wrappers for load inference, `file.Find`, hooks, net messages, timers, console commands, scripted classes, skins, gamemode inheritance, and similar systems should be expressed through annotations first. Hardcoded behavior is a last resort only when the required semantics truly cannot be represented with the current annotation model.
- Annotations are normally loaded by the extension or CLI as a library workspace, not bundled directly in the server. For local corpus validation, use `BENCH_ANNOTATIONS` or `glua_check --gmod-annotations`.

## High-Risk Areas
- Realm/load graph: keep changes close to `compilation/analyzer/gmod/mod.rs`, `db_index/gmod_load`, `db_index/gmod_infer`, `diagnostic/checker/gmod_realm_misuse.rs`, and `semantic` realm filters. Tests should cover realistic addon/gamemode paths, `include`, `AddCSLuaFile`, branch realms, meta/library files, and same function/member names defined in multiple realms.
- Shared loading-order rules: engine load roots, gamemode/scripted-class entrypoints, `autorun` variants, wrapper-based load edges, and `file.Find`-driven dynamic loads are centralized behavior. Fix those rules in the shared load-index/analyzer layer rather than sprinkling path special cases into diagnostics or LSP handlers.
- Annotation consumers and signature metadata: changes under `db_index/signature/**`, `compilation/analyzer/doc/type_ref_tags.rs`, `compilation/analyzer/gmod/mod.rs`, `handlers/gmod_string_context.rs`, and related call-role helpers can affect analyzer state, diagnostics, and multiple LSP features at once. Prefer one shared metadata-driven fix over patching each surface independently.
- Dynamic fields and flow narrowing: this area has many historical regressions. Preserve exact owners, source ranges, scope/region sensitivity, realm visibility, and edit stability. Relevant files include `compilation/analyzer/dynamic_field.rs`, `db_index/dynamic_field`, `semantic/member`, `semantic/infer/infer_index`, and flow/narrowing modules.
- Scripted classes and generated members: VGUI panels, `ENT`, `SWEP`, `TOOL`, `PLUGIN`, `AccessorFunc`, `NetworkVar`, Derma skins, class bases, and gamemode inheritance are modeled through indexed metadata and synthesized types/members. Many of these entry points are now driven by call-role metadata rather than name-specific handler code. Prefer extending `gmod_scripted_class_test.rs` and related realistic tests over small ad-hoc cases.
- Network diagnostics: `gmod_network` compares send/receive flows, read/write operation order, dynamic payload branches, and sender/receiver realms. Dynamic message names and dynamic read/write loops need conservative handling to avoid false positives.
- Diagnostics: diagnostics must consume precomputed shared data where available. `LuaDiagnostic::precompute_shared_data` and `SharedDiagnosticData` avoid per-file workspace rescans. `glua_check` uses `diagnose_file_with_shared`; do not regress CLI batch performance by adding per-file global scans.
- LSP handlers: definition, completion, hover, references, code lens, and custom string contexts should use analyzer metadata and shared helpers. If several handlers need the same fact, add or extend one shared API in `glua_code_analysis` or a shared handler helper instead of teaching each handler separately.
- Config/schema/docs: user-facing config changes must update config structs, schema, generated schema, and Mintlify docs together.

## Common Mistakes To Avoid
- Do not hide expensive analysis with new budget-style caps or broad work-skipping flags. Fix the algorithm, add relevance prefilters, cache/precompute, parallelize read-only per-file work, or move data into an index.
- Do not suppress false-positive diagnostics by turning off valid checks. Trace the wrong type/realm/member evidence to its source and fix the inference, index, or checker.
- Do not hardcode API behavior in multiple handlers when it belongs in annotations or indexed metadata. If a wrapper/helper should behave like a built-in across features, teach the shared annotation/index layer once instead of adding name checks in completion, hover, diagnostics, code lens, and definitions separately. Always prefer annotation led inference over name-based checks - we want our language server to be annotation driven.
- Do not let several features answer the same semantic question independently. If completion, hover, references, diagnostics, and code lens all need the same realm/load/wrapper fact, centralize it behind one shared API and make every caller use that.
- Do not treat `@call_arg` and related metadata as a narrow docs/testing feature. They are load-bearing for wrapper-aware string contexts, completion, navigation, code lenses, scripted-class synthesis, load indexing, realm inference, and some guard/narrowing behavior.
- Do not add new one-off recognizers for hook/net/load/VGUI/Derma/NetworkVar/class-base/gamemode/file-find patterns without first checking whether an existing annotation domain or signature metadata attribute should model it.
- Do not reimplement Garry's Mod loading-order rules from scratch in downstream code. If `autorun`, gamemode roots, scripted entities/weapons, `vgui`, `effects`, `stools`, `AddCSLuaFile`, `IncludeCS`, `require`, or dynamic `file.Find` loader loops are behaving incorrectly, fix the shared load-index/analyzer logic rather than layering another heuristic on top. Avoid repetition of work by using shared logic and caching where possible.
- Do not collapse realm-specific declarations into one unqualified signature/member. Same names can be valid in server, client, shared, menu, library, and branch-narrowed contexts.
- Do not add whole-workspace scans inside per-file diagnostics, completions, hovers, or reference loops. Precompute once, use `SharedDiagnosticData`, or add an index.
- Do not introduce nondeterministic output from `HashMap` iteration in diagnostics, completions, code lenses, or tests. Sort by stable keys such as `FileId`, range, path, or name before observable output.
- Do not assume no-op file opens are harmless. The code intentionally skips costly reindexing only when the index is already built, and rebuilds when it was cleared.
- Do not treat the benchmark harness as a diagnostics oracle. Use `glua_check` JSON output to compare corpus diagnostics.

## Config And Schema
- Config discovery is `.gluarc.json` first and exclusive. Only when it is absent, fall back to `.luarc.json`, `.emmyrc.json`, then `.emmyrc.lua` in that order.
- New config options usually require changes under:
  - `crates/glua_code_analysis/src/config/**`
  - `crates/glua_code_analysis/resources/schema.json`
  - `docs/mintlify/configuration/**` when user-facing
- After config/schema changes, run `cargo run --bin schema_json_gen` and verify it leaves no unintended diff.
- Gamemode base auto-detection scans actual workspace folders, with the main path as fallback. Do not change it to scan the config file directory.
- CLI/editor annotation loading has several entry points: `glua_ls --gmod-annotations-path`, extension-provided paths, `gmod.annotationsPath`, `gmod.autoLoadAnnotations`, and `glua_check --gmod-annotations`.

## Testing Patterns
- Prefer failing tests before production changes, especially for diagnostics, realm behavior, dynamic fields, inference, and performance-sensitive regressions.
- Most analysis tests live under `crates/glua_code_analysis/src/compilation/test/`, `diagnostic/test/`, or `semantic/**/test.rs`.
- Use `VirtualWorkspace` from `crates/glua_code_analysis/src/test_lib/mod.rs` for semantic and diagnostic tests. Use `def_files` and realistic addon/gamemode paths when behavior depends on load order, realm, or workspace layout.
- For tests that rely on annotation-driven call roles or guard metadata, load the right builtins/fixtures instead of reintroducing fake hardcoded behavior in the test itself. `def_gmod_call_arg_builtins()` is the standard fixture for call-role-driven systems.
- When changing annotation consumers, add coverage for both the metadata ingestion layer and at least one downstream behavior that consumes it, such as diagnostics, semantic inference, string contexts, code lenses, completion, or definitions.
- Prefer existing realistic test modules: `gmod_realm_misuse_test.rs`, `gmod_network_test.rs`, `gmod_systems_test.rs`, `gmod_annotation_shape_test.rs`, `gmod_scripted_class_test.rs`, `gmod_realm_hook_test.rs`, and related dynamic-field or undefined-field regression tests.
- Repository tests often use `googletest::prelude::*` and `#[gtest]`; prefer matcher assertions (`assert_that!`, `expect_that!`, `verify_that!`) where the local module already follows that style.
- For diagnostics snapshots across files, use the helper paths in `test_lib` such as `diagnostics_to_snapshot_set` and `diagnose_file_with_shared` patterns.

## Performance Rules
- Performance work should be backed by profiling or a targeted before/after benchmark. Use `GLUALS_PROFILE=1` for phase-level profiling logs and `cargo run --release -p benchmark` for large-workspace timing.
- Prefer indexed/cached structures over repeated AST/VFS scans. Use `FxHashMap`/`FxHashSet` where hot paths already use rustc-hash.
- Parallelize only read-only per-file collection with deterministic merge order. Existing examples include `GmodPreAnalysisPipeline`, flow binding, call-site param collection, and dynamic-field collection.
- Keep incremental behavior in mind: deleting, touching, or opening files can invalidate cross-file caches, shared diagnostics, load graphs, and dynamic-field visibility.
- Default `release` is kept fast for local iteration. Shipped/CI optimized binaries use `cargo build --profile dist` for thin LTO and single codegen unit.

## Commands
- Full tests: `cargo test`.
- Focused analyzer tests: `cargo test -p glua_code_analysis`.
- Focused test by name: `cargo test -p glua_code_analysis <test_name>`.
- CI-equivalent lint: `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- Format: `cargo fmt --all`.
- Pre-commit hygiene: `pre-commit run --all --hook-stage manual`.
- Build all release binaries: `cargo build --release`.
- Build one package: `cargo build --release -p glua_ls`, `cargo build --release -p glua_check`, or `cargo build --release -p glua_doc_cli`.
- Benchmark: `cargo run --release -p benchmark`; requires `BENCH_CODEBASE` and `BENCH_ANNOTATIONS`.
- Corpus diagnostics example on Windows:
  `cargo run --release -p glua_check -- --output-format json --gmod-annotations "%BENCH_ANNOTATIONS%" --output diagnostics.json "%BENCH_CODEBASE%"`
- Docs commands run from `docs/mintlify`: `mint dev` and `mint broken-links`.

## Release And Git
- CI uses stable Rust with `rustfmt` and `clippy`; `.clippy.toml` intentionally raises parser/analyzer complexity thresholds.
- PR/push CI gates clippy, tests, and schema freshness. macOS build artifacts are experimental/non-blocking.
- Release tags are plain `x.y.z`. Stable releases use patch `.0` and publish crates in the order encoded in `.github/workflows/build.yml`.
- Never push unless the user explicitly asks. Keep unrelated user changes intact.
- Commits are expected after each logical, verified change group unless the user asks not to commit.

## Agent Workflow Expectations
- Start Rust/code work by loading the `rust-best-practices` skill.
- Maintain a todo list for non-trivial work and keep it updated as tasks change.
- Work end to end by default: inspect, implement, verify, and summarize concrete results.
- If expected Garry's Mod Lua behavior is unclear, ask before choosing generic Lua behavior.
