<p align="center">
  <img src="https://raw.githubusercontent.com/Pollux12/vscode-gmod-glua-ls/refs/heads/main/res/gmod-glua-ls.png" width="128" alt="Garry's Mod Language Server icon">
</p>

<h1 align="center">gmod-glua-ls</h1>

<p align="center">
  Core Language Server for Garry's Mod Lua (gLua), written in Rust.
</p>


<p align="center">
  <a href="https://marketplace.visualstudio.com/items?itemName=Pollux.gmod-glua-ls">
    <img src="https://img.shields.io/visual-studio-marketplace/v/Pollux.gmod-glua-ls.png?style=flat-square&label=VSCode" alt="VSCode version">
  </a>
  <a href="https://github.com/Pollux12/gmod-glua-ls/releases">
    <img src="https://img.shields.io/github/v/release/Pollux12/gmod-glua-ls.png?style=flat-square&label=gLuaLS" alt="Language server version">
  </a>
  <a href="https://github.com/Pollux12/gmod-luals-addon/tree/gluals-annotations">
    <img src="https://img.shields.io/github/last-commit/Pollux12/gmod-luals-addon/gluals-annotations.png?style=flat-square&label=Annotations%20Updated" alt="Annotations updated">
  </a>
</p>

<p align="center">
  <a href="https://gluals.arnux.net/">Documentation</a>
  ·
  <a href="https://github.com/Pollux12/gmod-glua-ls/issues">Issues</a>
  ·
  <a href="https://github.com/Pollux12/vscode-gmod-glua-ls">VSCode Extension</a>
</p>

> [!IMPORTANT]
> This is an early release. There may be some minor bugs, please report any issues you run into. You should be able to resolve most issues via the config system.
> Report bugs or suggest features here: https://github.com/Pollux12/gmod-glua-ls/issues

This repository contains the core language server and backend tooling that power the **[VSCode extension](https://github.com/Pollux12/vscode-gmod-glua-ls)**.

You can find the VSCode extension and a full list of features here:
https://marketplace.visualstudio.com/items?itemName=Pollux.gmod-glua-ls

---

## ⚡ Performance & Architecture

* **Rust-Powered Backend:** Delivers fast indexing with a minimal memory footprint - over 10x quicker on large projects while delivering more features.
* **Full Language Server**: Includes everything you'd expect from a language server, such as syntax highlighting, diagnostics, symbol renaming, type resolution, goto, formatting and more.
* **Shared Foundation:** This repo is the backend that powers the VSCode extension and other editor integrations built around the language server.

## 🧠 Garry's Mod Specific Features

* **Class Resolution:** Automatic mapping for classes such as `ENT`, `SWEP`, `TOOL`, `PLUGIN` and others. NetworkVars, AccessorFuncs and VGUI panels are all registered as well.
* **Realm Awareness:** Analyses file prefixes (`sv_`, `cl_`, `sh_`) and `include()` chains. Generates real-time diagnostics for cross-realm function calls (e.g. calling a clientside method on the server). Delivers realm-aware suggestions by filtering autocomplete based on realm.
* **Network Validation:** Parses and validates `net.Start`, `net.Receive` and other net library usages, catching mismatched payloads, read/write order errors, and delivering enhanced autocomplete.
* **Smart Hook Integration:** Intelligent autocomplete and signature resolution for all hooks, `GM:` overrides, and custom `---@hook` annotations. Automatically detects and registers new custom hooks in addition to those parsed from the wiki.
* **Class Explorer & Templates:** Dedicated side-panel to easily reference key classes (Entities, Weapons, VGUI, Plugins) and workspace resources (Materials, Sounds) alongside a configurable template system for easy creation.

For a full list of features, see the VSCode extension page here: https://github.com/Pollux12/vscode-gmod-glua-ls

---

## 🔌 Editors & Installation

### VSCode

The recommended setup is the **[VSCode extension](https://marketplace.visualstudio.com/items?itemName=Pollux.gmod-glua-ls)**.

That is the product most users want. It wraps this language server with:

* automatic annotation downloads and updates,
* debugger setup and tooling,
* custom settings UI,
* workspace helpers and editor-specific features.

### Other Editors

Any LSP-compatible editor can use this repository's `glua_ls` binary directly.

Install from Cargo:

```bash
cargo install glua_ls glua_check
```

Or build from source:

```bash
git clone https://github.com/Pollux12/gmod-glua-ls.git
cd gmod-glua-ls
cargo build --release
```

Binary location: `target/release/glua_ls` (or `glua_ls.exe` on Windows)

You can also point the upstream EmmyLua extension at the `glua_ls` binary if you only want the language server portion.

### Zed

Support for Zed is being worked on, but it will remain more limited than VSCode because Zed currently exposes less editor integration surface.

## 🐞 Debugger & Tooling

This repo also contains the backend pieces used by the debugger and supporting tooling, but the easiest way to use those features is still through the VSCode extension.

The debugger is intended for local development environments, especially SRCDS-based addon or gamemode workflows.

## 📚 Configuration & Annotations

Configuration is easiest through the VSCode extension UI, but the underlying options are documented here as well:

* [Configuration Reference](./docs/config.md)
* [Annotation Documentation](./docs/annotations/README.md)

Standard EmmyLua / LuaCATS annotations are supported, along with GMod-specific additions such as `---@hook`.

See updated documentation here: https://gluals.arnux.net

## Troubleshooting

If you are using VSCode, avoid running multiple competing Lua language extensions at the same time. For Garry's Mod work, this should generally be the only Lua language server / debugger stack enabled.

If you are working outside the standard `garrysmod/addons` or `garrysmod/gamemodes` layout, some features may need manual configuration. If that setup does not work cleanly, please open an issue and include your folder structure.

---

This is a hard fork of [EmmyLua Analyzer Rust](https://github.com/CppCXY/emmylua-analyzer-rust), maintained specifically for Garry's Mod GLua.
The original EmmyLua project does not support plugins, nor does it have any plan for them, making it difficult to fully adapt for Garry's Mod. This project contains significant changes from the original and only works for Garry's Mod GLua.
While LuaLS has plugin support, it was annoyingly slow to use. Many features here are based on my [LuaLS plugin](https://github.com/Pollux12/gmod-luals-addon).
