# Project Guidelines

## Project Context
- This repository is primarily a **Garry's Mod Language Server** fork of EmmyLua Analyzer Rust.
- Default behavior should optimize for GLua/GMod workflows first, not generic Lua LS behavior.
- When requirements are ambiguous, choose GMod-aware behavior and config defaults unless a task explicitly requests otherwise.
- GitHub Repository: [@Pollux12/gmod-glua-ls](https://github.com/Pollux12/gmod-glua-ls)

## Architecture (Read This First)
- Workspace layout by responsibility:
  - `crates/glua_parser`: parser + CST/AST foundation (`rowan`-based).
  - `crates/glua_parser_desc`: parser descriptor metadata used by tooling/codegen.
  - `crates/glua_code_analysis`: semantic core (compilation DB, diagnostics, VFS, config, GMod inference/indexes).
  - `crates/glua_code_style`: This is currently NOT used - formatting and style is currently done via `vendor\emmylua_codestyle` C++ Lua formatter.
  - `crates/glua_diagnostic_macro`: proc-macro crate for diagnostics definitions.
  - `crates/glua_ls`: LSP server runtime and handlers.
  - `crates/glua_check`: CLI diagnostics runner.
  - `crates/glua_doc_cli`: annotation-to-docs generator.
  - `crates/schema_to_glua`: schema-to-annotation conversion helpers.
  - `tools/schema_json_gen`: schema generator binary used by CI drift checks.
  - `tools/benchmark`: performance benchmarking tool that measures indexing and diagnostics on real codebases.
- Runtime flow is parser → analysis/indexing → LS/CLI consumers.
- Analysis pipeline (in `crates/glua_code_analysis/src/compilation/analyzer/mod.rs`):
  `Decl → Doc → Flow → Lua → Gmod → AccessorFunc synthesis → UnResolve → [DynamicField if inferDynamicFields]`
- Note that we've added multi-workspace support for our language server. You need to make sure that any changes you make will support workspaces with different configurations, with each being isolated.

## GMod-Specific Logic
- This is a Garry's Mod specific language server with no backwards compatibility requirements for use outside of Garry's Mod. All GMod behaviour needs to be treated as first-class, such as non-standard operators, hook/realm behavior, scripted class patterns, Garry's Mod annotation library treated same as stdlib, etc.
- GMod inference implementation lives in `crates/glua_code_analysis/src/compilation/analyzer/gmod/mod.rs`.
- GMod metadata persistence is in `crates/glua_code_analysis/src/db_index/`, e.g:
  - `gmod_class/` — scripted class metadata
  - `gmod_infer/` — GMod inference results
  - `gmod_network/` — net.Start/net.Receive flow tracking for cross-realm diagnostics
  - `dynamic_field/` — dynamically-assigned field tracking (when `gmod.inferDynamicFields` enabled)
**Obscure Patterns:**
- **Realm Inference**: `---@realm` annotation (highest priority) > filename/dir detection (`cl_`/`sv_`/`sh_` prefixes, `/lua/server/` parent dirs) > API dependency hints. Block-level narrowing via `if CLIENT`/`if SERVER` applies within files (stored as `branch_realm_ranges`).
- **Accessor Synthesis**: `AccessorFunc()` calls synthesize `GetPropertyName()` and `SetPropertyName()` methods on the target class. The `---@accessorfunc` annotation marks a function as an accessor generator (providing metadata like custom param index).
- **Network Profiling**: Matches `net.Start(name)` writes with `net.Receive(name)` reads across files/realms to diagnose type/order mismatches.
- **Scripted Classes**: Automatically detects ENT/SWEP/EFFECT classes based on file path patterns (e.g., `entities/**`).

## Config and Schema Rules
- Configuration entry points are `.gluarc.json` (exclusive priority when present), otherwise `.luarc.json` → `.emmyrc.json` → `.emmyrc.lua`.
- VSCode extension provides schema and custom settings menu for editing `.gluarc.json` config files.
- For new config options, update all of these together:
  - code: `crates/glua_code_analysis/src/config/**`
  - schema: `crates/glua_code_analysis/resources/schema.json`
  - docs: `docs/config.md` (and any detailed docs under `docs/config/` if applicable)
- Run schema generation after any config changes: `cargo run --bin schema_json_gen`
- Make sure all config is documented in `docs/config.md`.

## Build, Test, and Validation Commands
- Build all: `cargo build --release`
- Build one crate: `cargo build --release -p glua_ls` (or `glua_check`, `glua_doc_cli`)
- Run benchmark: `cargo run --release -p benchmark` (requires `BENCH_CODEBASE` and `BENCH_ANNOTATIONS` env vars)
- Test all: `cargo test`
- Focused loop (common): `cargo test -p glua_code_analysis`
- Lint (CI-equivalent): `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- Format: `cargo fmt --all`
- Pre-commit checks: `pre-commit run --all --hook-stage manual`
- Spell check in CI: `typos`
- Cargo is installed in local system and is within path. If you get an error stating it is missing, try use direct path workaround e.g: `& "$env:USERPROFILE\.cargo\bin\cargo.exe"` but ONLY if cargo does not work by itself.

## Testing Patterns (How This Repo Verifies Behavior)
- We use the standard Rust testing harness with [googletest-rust](https://github.com/google/googletest-rust/). Prefer `#[gtest]` over `#[test]` in repository test modules.
- In test modules, import `googletest::prelude::*` and prefer matcher-style assertions (`assert_that!`, `expect_that!`, `verify_that!`) instead of introducing new `assert_eq!`/`assert!` where practical.
- Use the `check!` helper where available to convert `Result`/`Option` into `googletest::Result` with useful location context.
- Many semantic/diagnostic tests use `VirtualWorkspace` from `crates/glua_code_analysis/src/test_lib/mod.rs`.
- When changing analysis behavior, add/adjust tests near the affected subsystem in:
  - `crates/glua_code_analysis/src/compilation/test/`
  - `crates/glua_code_analysis/src/diagnostic/test/`
  - `crates/glua_code_analysis/src/semantic/**/test.rs`
- For GMod-specific changes, prioritize extending existing `gmod_*` tests rather than adding isolated ad-hoc coverage.
- For GMod realm/path-sensitive tests, use realistic addon or gamemode style paths.

**IMPORTANT**
- Performance is extremely important. This language server is designed to run in large workspaces with potentially thousands of files. Always consider the performance implications of your changes, and prefer efficient algorithms and data structures.
- You shouldn't read files unless you're confident that file is relevant - use semantic search and other search tools to narrow down relevant files before any read operation. If you think you'll need to read a file multiple times, read the entire file once to save on tool calls. If you already have a file or relevant code in context, you don't need to read it again unless it has been modified.
- Do not use your terminal tool unless no other tool can accomplish the same task. Specialised tools will always be more effective than generic terminal commands. Especially for search, always prefer semantic search tools first.
- All documentation, including this file, should be treated as non-comprehensive and potentially outdated. Always verify and ground knowledge by checking the code itself. This document is a guide to help you get oriented and understand the general structure and patterns of the repository.
