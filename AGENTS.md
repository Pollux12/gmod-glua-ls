# Project Guidelines

## Project Context
- This repository is a **Garry's Mod Language Server** fork of EmmyLua Analyzer Rust modified for Garry's Mod specific behaviour.
- Default behavior should optimize for GLua/GMod workflows first, not generic Lua LS behavior.
- When requirements are ambiguous, choose GMod-aware behavior and config defaults unless a task explicitly requests otherwise.
- GitHub Repository: [@Pollux12/gmod-glua-ls](https://github.com/Pollux12/gmod-glua-ls)

## GMod-Specific Logic
- This is a Garry's Mod specific language server with no backwards compatibility requirements for use outside of Garry's Mod. All GMod behaviour needs to be treated as first-class, such as non-standard operators, hook/realm behavior, scripted class patterns, Garry's Mod annotation library treated same as stdlib, etc.
- We have realm awareness based on various indicators such as realm annotations, but also automatic file path inference, to try and accurately best detect the realm of all functions, methods and variables. You need to make sure all features are realm aware where applicable, such as autocomplete which shouldn't suggest things from another realm, etc.
- Always prefer to do things at the annotation level rather than hardcoding. Always look to see if something can be done at the annotation level, as this increases accuracy (since it'll be attached to the actual function/method), reduces repetition of code and helps add better generic support for user defined annotations.
- Annotations are loaded by the extension, which is why annotations are not available to the language server directly, although they can be loaded manually. You are allowed to and encouraged to make annotation level changes where required by editing the annotation repo, which can likely be found in a folder adjacent to this project, or can be found via the $BENCH_ANNOTATIONS env var which would contain the path for the output folder where annotations are generated.

## Config and Schema Rules
- Configuration entry points are `.gluarc.json` (exclusive priority when present), otherwise `.luarc.json` → `.emmyrc.json` → `.emmyrc.lua`.
- VSCode extension provides schema and custom settings menu for editing `.gluarc.json` config files.
- For new config options, update all of these together:
  - code: `crates/glua_code_analysis/src/config/**`
  - schema: `crates/glua_code_analysis/resources/schema.json`
  - docs (if applicable)
- Run schema generation after any config changes: `cargo run --bin schema_json_gen`

## Build, Test, and Validation Commands
- Build all: `cargo build --release`
- Build one crate: `cargo build --release -p glua_ls` (or `glua_check`, `glua_doc_cli`)
- Run benchmark: `cargo run --release -p benchmark` (requires `BENCH_CODEBASE` and `BENCH_ANNOTATIONS` env vars)
- Test all: `cargo test`
- Focused loop (common): `cargo test -p glua_code_analysis`
- Lint (CI-equivalent): `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- Format: `cargo fmt --all`
- Pre-commit checks: `pre-commit run --all --hook-stage manual`
- Cargo is installed in local system and is within path. If you get an error stating it is missing, try use direct path workaround e.g: `& "$env:CARGO_HOME\bin\cargo.exe"` but ONLY if cargo does not work by itself.

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
- Performance is extremely important. This language server is designed to run in large workspaces with potentially thousands of files. Always consider the performance implications of your changes, and prefer efficient algorithms and data structures. We do not want performance hacks like flow budgets - instead, we want proper optimisations such as more efficient algorithms, better data structures, reducing repetition of work, etc...
- You shouldn't read files unless you're confident that file is relevant - use semantic search and other search tools to narrow down relevant files before any read operation. If you think you'll need to read a file multiple times, read the entire file once to save on tool calls. If you already have a file or relevant code in context, you don't need to read it again unless it has been modified.
- Do not use your terminal tool unless no other tool can accomplish the same task. Specialised tools will always be more effective than generic terminal commands. Especially for search, always prefer semantic search tools first.
- Always start by loading the `rust-best-practices` skill.