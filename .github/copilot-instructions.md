# Project Guidelines

## Project Context
- This repository is primarily a **Garry's Mod Language Server** fork of EmmyLua Analyzer Rust.
- Default behavior should optimize for GLua/GMod workflows first, not generic Lua LS behavior.
- When requirements are ambiguous, choose GMod-aware behavior and config defaults unless a task explicitly requests otherwise.
- GitHub Repository: [@Pollux12/gmod-glua-ls](https://github.com/Pollux12/gmod-glua-ls)

## Architecture (Read This First)
- Workspace layout by responsibility:
  - `crates/emmylua_parser`: parser + CST/AST foundation (`rowan`-based).
  - `crates/emmylua_parser_desc`: parser descriptor metadata used by tooling/codegen.
  - `crates/emmylua_code_analysis`: semantic core (compilation DB, diagnostics, VFS, config, GMod inference/indexes).
  - `crates/emmylua_code_style`: code-style support consumed by analysis/check flows.
  - `crates/emmylua_diagnostic_macro`: proc-macro crate for diagnostics definitions.
  - `crates/emmylua_ls`: LSP server runtime and handlers.
  - `crates/emmylua_check`: CLI diagnostics runner.
  - `crates/emmylua_doc_cli`: annotation-to-docs generator.
  - `crates/schema_to_emmylua`: schema-to-annotation conversion helpers.
  - `tools/schema_json_gen`: schema generator binary used by CI drift checks.
- Runtime flow is parser → analysis/indexing → LS/CLI consumers.
- Analysis pipeline order is in `crates/emmylua_code_analysis/src/compilation/analyzer/mod.rs`.
  - GMod analysis is conditionally inserted via `gmod::GmodAnalysisPipeline` when `emmyrc.gmod.enabled` is true.
- GMod inference implementation lives in `crates/emmylua_code_analysis/src/compilation/analyzer/gmod/mod.rs`.
- GMod metadata persistence is in `crates/emmylua_code_analysis/src/db_index/` (`gmod_*` indexes).
- Note that we've added multi-workspace support for our language server. You need to make sure that any changes you make will support workspaces with different configurations, with each being isolated.

## Work Routing (Where To Implement Changes)
- Parser/grammar/AST/CST: `crates/emmylua_parser/src/`.
- Type inference, diagnostics, indexing, GMod realm/hooks/system metadata: `crates/emmylua_code_analysis/src/`.
- LSP protocol behavior, workspace lifecycle, watched files, client capabilities: `crates/emmylua_ls/src/`.
- CLI diagnostics behavior/output/arg parsing: `crates/emmylua_check/src/`.
- Documentation generation flow/templates: `crates/emmylua_doc_cli/src/` + `crates/emmylua_doc_cli/template/`.

## GMod-First Invariants (Do Not Regress)
- Preserve defaults from `crates/emmylua_code_analysis/src/config/configs/gmod.rs`:
  - `gmod.enabled = true`
  - `gmod.defaultRealm = shared`
  - scripted class include defaults: `entities/**`, `weapons/**`, `effects/**`, `weapons/gmod_tool/stools/**`, `plugins/**`
- Preserve hook/realm behavior described in `docs/config.md` and `docs/annotations/hook.md`:
  - method hooks: `GM:*`, `GAMEMODE:*`, `PLUGIN:*`, `SANDBOX:*`
  - hook API parsing: `hook.Add`, `hook.Run`, `hook.Call`
  - annotation hooks: `---@hook`
  - realm inference from filename + dependency/call signals
- Do not introduce generic-Lua fallback behavior that weakens GMod inference unless explicitly requested.
- If changing GMod hook/realm logic, keep tests aligned:
  - `crates/emmylua_code_analysis/src/compilation/test/gmod_realm_hook_test.rs`
  - `crates/emmylua_code_analysis/src/compilation/test/gmod_scripted_class_test.rs`
- All non-standard GMod operators should be treated as standard, with first-class support in completion/signature/diagnostics (C style operators)

## Config and Schema Rules
- Configuration entry points are `.gluarc.json` (exclusive priority when present), otherwise `.luarc.json` → `.emmyrc.json` → `.emmyrc.lua`.
- LS config priority is implemented in `crates/emmylua_ls/src/context/workspace_manager.rs`:
  - global home config → global config-dir (`gluals`) config → `$GLUALS_CONFIG` → local workspace config.
  - for merged LS config, the first workspace root with local config is preferred for scalar fields; selected list-like fields are extend-unique merged from additional roots.
- For new config options, update all of these together:
  - code: `crates/emmylua_code_analysis/src/config/**`
  - schema: `crates/emmylua_code_analysis/resources/schema.json`
  - docs: `docs/config.md` (and any detailed docs under `docs/config/` if applicable)
- Keep schema generation clean: `cargo run --bin schema_json_gen` must not leave a git diff.
- Make sure all config is documented in `docs/config.md`.

## Build, Test, and Validation Commands
- Build all: `cargo build --release`
- Build one crate: `cargo build --release -p emmylua_ls` (or `emmylua_check`, `emmylua_doc_cli`)
- Test all: `cargo test`
- Focused loop (common): `cargo test -p emmylua_code_analysis`
- Lint (CI-equivalent): `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- Format: `cargo fmt --all`
- Pre-commit checks: `pre-commit run --all --hook-stage manual`
- Spell check in CI: `typos`
- Cargo is installed in local system and is within path. If you get an error stating it is missing, try use direct path workaround e.g: `& "$env:USERPROFILE\.cargo\bin\cargo.exe"` but ONLY if cargo does not work by itself.

## Code Style and Conventions
- Rust edition is `2024`; `rustfmt.toml` uses `max_width = 100`, 4 spaces.
- Follow workspace lint policy in `Cargo.toml` + thresholds in `.clippy.toml`.
- Match crate structure style:
  - private `mod` declarations
  - deliberate `pub use` re-exports from crate roots
  - examples: `crates/emmylua_parser/src/lib.rs`, `crates/emmylua_code_analysis/src/lib.rs`
- Keep i18n pattern consistent where present:
  - `rust_i18n::i18n!("./locales", fallback = "en")`
  - used by parser/analysis/ls crate roots
- Prefer crate-local boundaries over cross-crate leaking of internals.

## Integration Points
- LSP protocol: `lsp-server` + `emmy_lsp_types` in `emmylua_ls`.
- Async/runtime and IO: `tokio`, `tokio-util`, `notify`.
- Parser tree infra: `rowan`.
- Config/data formats: `serde`, `serde_json`, `schemars`.
- Schema-to-annotation path: `schema_to_emmylua` consumed by analysis crate.
- External formatter options are documented in `docs/config.md` (`format` section).

## Security and High-Risk Areas
- `emmylua_code_analysis` denies panic/unwrap patterns in non-test builds (`clippy::panic`, `clippy::unwrap_used`, etc.).
- `EmmyLuaAnalysis` has manual thread-safety boundaries (`unsafe impl Send/Sync`) in `crates/emmylua_code_analysis/src/lib.rs`; treat related changes as high risk.
- `update_schema` fetches remote schema URLs via `reqwest`; treat network/file schema sources as untrusted input.

## Testing Patterns (How This Repo Verifies Behavior)
- We use the standard Rust testing harness with [googletest-rust](https://github.com/google/googletest-rust/). Prefer `#[gtest]` over `#[test]` in repository test modules.
- In test modules, import `googletest::prelude::*` and prefer matcher-style assertions (`assert_that!`, `expect_that!`, `verify_that!`) instead of introducing new `assert_eq!`/`assert!` where practical.
- Use the `check!` helper where available to convert `Result`/`Option` into `googletest::Result` with useful location context.
- Many semantic/diagnostic tests use `VirtualWorkspace` from `crates/emmylua_code_analysis/src/test_lib/mod.rs`.
- When changing analysis behavior, add/adjust tests near the affected subsystem in:
  - `crates/emmylua_code_analysis/src/compilation/test/`
  - `crates/emmylua_code_analysis/src/diagnostic/test/`
  - `crates/emmylua_code_analysis/src/semantic/**/test.rs`
- For GMod-specific changes, prioritize extending existing `gmod_*` tests rather than adding isolated ad-hoc coverage.
- For GMod realm/path-sensitive tests, use realistic addon or gamemode style paths.

**IMPORTANT**
When using your file read tool, read entire files unless the file is extremely large - in this case, read in 1000 line chunks. Do not read files in tiny chunks, this will be slower and make you lose important context.
