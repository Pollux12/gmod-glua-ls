# Agent Notes

## Project Shape
- This is the Rust backend for GLuaLS, a hard fork of EmmyLua Analyzer Rust for Garry's Mod GLua; prefer GMod-correct behavior over generic Lua language-server compatibility.
- Workspace crates live under `crates/*`; tools live under `tools/*`.
- `crates/glua_code_analysis` is the static-analysis engine; `crates/glua_ls` is the LSP server; `crates/glua_check` is the CLI diagnostics runner; `crates/glua_parser` owns parsing; `crates/glua_doc_cli` generates docs.
- The VSCode extension supplies annotations and editor UI outside this repo; server changes that depend on shipped annotations may also need changes in the annotations or extension repos.

## GLua/GMod Rules
- Realm awareness is first-class: path inference, realm annotations, include chains, completions, diagnostics, and symbol behavior must not leak client/server/shared APIs incorrectly.
- Prefer annotation-level modeling over hardcoded LS behavior when the fact belongs to a function, hook, class, or annotation library entry.
- Annotations are normally loaded by the extension, not bundled directly in the server. For local validation, use `BENCH_ANNOTATIONS` or pass annotations to `glua_check` with `--gmod-annotations`.
- False-positive diagnostics should be fixed at the root cause without suppressing valid diagnostics.

## Config And Schema
- Config discovery is `.gluarc.json` first and exclusive; only when it is absent, fall back to `.luarc.json`, `.emmyrc.json`, then `.emmyrc.lua`.
- New config options must update code under `crates/glua_code_analysis/src/config/**`, `crates/glua_code_analysis/resources/schema.json`, and the Mintlify docs under `docs/mintlify/configuration/**` when user-facing.
- After config/schema changes, run `cargo run --bin schema_json_gen` and verify it leaves no unintended diff.

## Commands
- Full tests: `cargo test`.
- Focused crate tests: `cargo test -p glua_code_analysis` or `cargo test -p glua_code_analysis <test_name>`.
- CI-equivalent lint: `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- Format: `cargo fmt --all`; pre-commit runs this plus file hygiene via `pre-commit run --all --hook-stage manual`.
- Build all release binaries: `cargo build --release`; build one package with `cargo build --release -p glua_ls` or `-p glua_check`.
- Benchmark: `cargo run --release -p benchmark`; requires `BENCH_CODEBASE` and `BENCH_ANNOTATIONS` to point at existing directories.
- Compare corpus diagnostics with `glua_check` JSON output, not the benchmark harness, for example `cargo run --release -p glua_check -- --output-format json --gmod-annotations "%BENCH_ANNOTATIONS%" --output diagnostics.json "%BENCH_CODEBASE%"` on Windows.
- Docs site lives in `docs/mintlify`; run Mintlify commands from that directory (`mint dev`, `mint broken-links`).

## Testing Patterns
- Prefer adding failing tests before production LS changes, especially for corpus diagnostic fixes.
- Repository tests commonly use `googletest::prelude::*` and `#[gtest]`; prefer matcher assertions (`assert_that!`, `expect_that!`, `verify_that!`) over new raw `assert_eq!`/`assert!` where practical.
- Use `VirtualWorkspace` from `crates/glua_code_analysis/src/test_lib/mod.rs` for semantic and diagnostic behavior.
- Put analysis tests near the affected subsystem: `crates/glua_code_analysis/src/compilation/test/`, `crates/glua_code_analysis/src/diagnostic/test/`, or `crates/glua_code_analysis/src/semantic/**/test.rs`.
- For GMod behavior, extend realistic `gmod_*` tests and use addon/gamemode-style paths for realm-sensitive cases.

## Performance And Safety
- Performance work should be driven by profiling evidence and validated with tests or benchmarks.
- Do not add budget-style caps to hide expensive analysis; only true recursion, infinite-loop, cycle guards, or display/output truncation caps are acceptable.
- Large-workspace behavior matters: prefer efficient data structures and incremental/dependent-file work over whole-workspace rescans when possible.
- Keep the worktree clean after verified changes; do not commit or discard unrelated user changes.

## Release/CI Notes
- CI uses stable Rust with `rustfmt` and `clippy`; `.clippy.toml` raises parser/analyzer complexity thresholds intentionally.
- PR/push CI gates clippy, tests, and schema freshness. macOS checks/build artifacts are experimental and non-blocking.
- Release tags are plain `x.y.z`; stable releases use patch `.0` and publish crates in the order encoded in `.github/workflows/build.yml`.

## OpenCode
- Start Rust work by loading the `rust-best-practices` skill.
