# Garry's Mod GLua Language Server

> [!IMPORTANT]
> This is an early release, there may be some bugs or unexpected issues, such as some diagnostics being annoying with false positives or similar. You can turn any annoying diagnostics off or change their severity level in the config.

A fast, feature-rich language server for Garry's Mod Lua (GLua), written in Rust

You likely want the VSCode extension here: https://github.com/Pollux12/vscode-gmod-glua-ls

## Installation

### VSCode

The recommended way is to use the GLua extension: https://github.com/Pollux12/vscode-gmod-glua-ls

### Other Editors

Any LSP-compatible editor should work. Point your LSP client to the `glua_ls` binary.

### Build from Source

```bash
git clone https://github.com/Pollux12/gmod-glua-ls.git
cd gmod-glua-ls
cargo build --release
```

Binary location: `target/release/glua_ls` (or `glua_ls.exe` on Windows)

## Configuration

Configuration is primarily handled via a built-in menu within the VSCode extension

See [docs/config.md](./docs/config.md) for manual configuration options

## Annotations

Support for standard EmmyLua/LuaCATS annotations (`---@class`, `---@param`, etc.) plus GMod-specific additions such as `---@hook`.

See [docs/annotations/](./docs/annotations/) for the full reference.

## Credits

This is a hard fork of [EmmyLua Analyzer Rust](https://github.com/CppCXY/emmylua-analyzer-rust), maintained specifically for Garry's Mod GLua.
The original EmmyLua project does not support plugins, nor does it have any plan for any, making it difficult to fully adapt for Garry's Mod.
While LuaLS has plugin support, it was annoyingly slow to use. Many features here are based on my [LuaLS plugin](https://github.com/Pollux12/gmod-luals-addon).
