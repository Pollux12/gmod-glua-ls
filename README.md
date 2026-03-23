# Garry's Mod GLua Language Server

> [!IMPORTANT]
> This is an early release. There may be some minor bugs, please report any issues you run into! You should be able to resolve most issues via the config system (e.g. disabling diagnostics or changing folder paths).

A fast, feature-rich language server for Garry's Mod Lua (GLua), written in Rust.

You likely want the VSCode extension here: https://github.com/Pollux12/vscode-gmod-glua-ls

[Annotation Documentation](https://github.com/Pollux12/gmod-glua-ls/blob/main/docs/annotations/README.md)

---

## Installation

### VSCode (and related forks)

The recommended way is to use the GLua extension: https://github.com/Pollux12/vscode-gmod-glua-ls

Please see the VSCode extension repo for a full list of features available and installation process.

Many advanced features are currently exclusive to the VSCode extension and are not part of the language server.

### Zed

Support for Zed is being worked on, although it will have less features than VSCode, due to its API being more limited (can't create custom UI yet with Zed).

### Other Editors

> [!IMPORTANT]
> Many advanced features will be missing if used outside of the VSCode extension, with the language server only delivering basic functionality on its own.
> It is recommended to use the VSCode extension

Other editors can still make use of the language server portion.

Any LSP-compatible editor should work. Point your LSP client to the `glua_ls` binary.

Install the Cargo packages with:

```bash
cargo install glua_ls glua_check
```

You can also use the language server with the upstream EmmyLua extension by changing the language server binary path to point to glua_ls.

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

---

This is a hard fork of [EmmyLua Analyzer Rust](https://github.com/CppCXY/emmylua-analyzer-rust), maintained specifically for Garry's Mod GLua.
The original EmmyLua project does not support plugins, nor does it have any plan for any, making it difficult to fully adapt for Garry's Mod. This project contains significant changes from the original and only works for Garry's Mod GLua.
While LuaLS has plugin support, it was annoyingly slow to use. Many features here are based on my [LuaLS plugin](https://github.com/Pollux12/gmod-luals-addon).
