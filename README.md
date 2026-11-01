<p align="center">
  <img src="https://raw.githubusercontent.com/Pollux12/vscode-gmod-glua-ls/refs/heads/main/res/gmod-glua-ls.png" width="128" alt="Garry's Mod Language Server icon">
</p>

<h1 align="center">gmod-glua-ls</h1>

<p align="center">
  Core Language Server for Garry's Mod Lua (gLua), written in Rust.
</p>


<p align="center">
  <a href="https://marketplace.visualstudio.com/items?itemName=Pollux.gmod-glua-ls">
    <img src="https://img.shields.io/visual-studio-marketplace/v/Pollux.gmod-glua-ls?style=flat-square&label=VSCode" alt="VSCode version">
  </a>
  <a href="https://github.com/Pollux12/gmod-glua-ls/releases">
    <img src="https://img.shields.io/github/v/release/Pollux12/gmod-glua-ls?style=flat-square&label=gLuaLS" alt="Language server version">
  </a>
  <a href="https://github.com/Pollux12/annotations-gmod-glua-ls/tree/gluals-annotations">
    <img src="https://img.shields.io/github/last-commit/Pollux12/annotations-gmod-glua-ls/gluals-annotations?style=flat-square&label=Stable%20Annotations" alt="Stable annotations updated">
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

Install the extension from the [VS Code Marketplace](https://marketplace.visualstudio.com/items?itemName=Pollux.gmod-glua-ls).

---

## Features

* Completion, hover docs, diagnostics, go to definition, find references, rename and formatting
* Realm checks for client and server code
* Checks and completion for `net` messages and hooks
* Typed scripted classes such as `ENT`, `SWEP`, `TOOL` and Helix plugins, including NetworkVars and AccessorFuncs
* Fast analysis of large workspaces, written in Rust

See the [VSCode extension](https://github.com/Pollux12/vscode-gmod-glua-ls) for the full feature list with media.

---

## 🔌 Editors & Installation

### VSCode

The recommended setup is the **[VSCode extension](https://marketplace.visualstudio.com/items?itemName=Pollux.gmod-glua-ls)**.

It wraps this language server with:

* automatic annotation downloads and updates,
* debugger setup and tooling,
* custom settings UI and easier setup,
* workspace helpers and editor-specific features,
* many more QoL features.

Since this started as just a vscode extension, many features are tied to it, such as annotations which only automatically download if used via the extension. It is possible to manually pass an annotation path to the language server if you want to use it standalone, and future automatic setup via just the language server is planned.

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

You can also point the EmmyLua vscode extension at the `glua_ls` binary if you only want the language server portion, although this is not recommended.

### Zed

Support for Zed is being worked on, but it will remain more limited than VSCode because Zed currently exposes far less functionality for extensions.

## 🐞 Debugger & Tooling

This repo also contains the backend pieces used by the debugger and supporting tooling, but the easiest way to use those features is still through the VSCode extension.

The debugger is intended for local development environments, especially SRCDS-based addon or gamemode workflows.

## 📚 Configuration & Annotations

The language server is designed with defaults that should "just work" in most addons and gamemodes, when used via the VSCode extension. On its own, you will need to pass things like annotations manually to the language server, as they are currently managed by the extension.

Configuration is easiest through the VSCode extension's settings panel. Every option and annotation is documented at https://gluals.arnux.net:

* [Configuration](https://gluals.arnux.net/configuration/overview)
* [Annotations](https://gluals.arnux.net/annotations)

Standard EmmyLua / LuaCATS annotations are supported, however there are some key differences and additions to make it work better with Garry's Mod, such as new realm and hook annotations.

You can find the annotations repo here: https://github.com/Pollux12/annotations-gmod-glua-ls


## Troubleshooting

If you are using VSCode, avoid running multiple competing Lua language extensions at the same time. This language server is not compatible with "GLua Enhanced".

If you are working outside the standard `garrysmod/addons` or `garrysmod/gamemodes` layout, some features may need manual configuration. If that setup does not work cleanly, please open an issue and include your folder structure.

---

This project started as hard fork of [EmmyLua Analyzer Rust](https://github.com/CppCXY/emmylua-analyzer-rust), but has had significant changes since to make it work well with Garry's Mod.
Some features are based on my [LuaLS plugin](https://github.com/Pollux12/gmod-luals-addon).
