# Garry's Mod GLua Language Server

A fast, feature-rich language server for Garry's Mod Lua (GLua), written in Rust, based on [EmmyLua Analyzer Rust](https://github.com/CppCXY/emmylua-analyzer-rust).

> [!IMPORTANT]
> This is an early release, there may be some bugs or unexpected issues, such as some diagnostics being annoying with false positives or similar. You can turn any annoying diagnostics off or change their severity level in the config.

<!-- TODO: Add GIF showing autocomplete for GMod functions -->

## Features

<!-- TODO: Add screenshots for each major feature -->

**Realm Detection**

- Infers realm for all functions based on several patterns, with automatic realm tags and diagnostics on realm mismatch.

**Hook Autocomplete**

- Smart hook detection, autocomplete and annotations, including `GM:` or custom method hooks (use `---@hook` annotation).

**Advanced Class & Entity Support**

- Full support for `ENT`, `SWEP`, `TOOL`, and custom objects such as `PLUGIN`.
- Automatically adds getter/setter functions for `NetworkVar`, `AccessorFunc`, and more, with custom support via `---@accessorfunc` annotation
- Automatically adds definitions for all detected objects, such as class definitions for all entities and more.

**Dynamic Field Inference**

- Tracks fields dynamically set on various objects, such as Player, Entity, and more, to prevent you from having to annotation these built-in classes.
- Provides autocomplete and definitions for these fields.

**Speed**

- Takes seconds on large codebases vs LuaLS taking minutes.
- Uses significantly less memory vs LuaLS.
- This project was born out of frustration for how slow LuaLS can be.

## LSP Features (from EmmyLua)

Includes everything you'd expect from a Language Server:

- Autocomplete with snippets
- Go to definition / Find references
- Hover documentation
- Signature help
- Rename refactoring
- Diagnostics (errors, warnings, hints)
- Inlay hints (parameter names, types)
- Code actions and code lens
- Code formatting

See more at: [EmmyLua Feature List](https://github.com/EmmyLuaLs/emmylua-analyzer-rust/blob/main/docs/features/features_EN.md#-code-formatting)

## Installation

### VS Code

The recommended way is to use the GLua extension (coming soon). You can also configure the [EmmyLua extension](https://marketplace.visualstudio.com/items?itemName=tangzx.emmylua) to use this language server binary - change the language server path in VSCode setting under the EmmyLua category.

### Other Editors

Any LSP-compatible editor should work. Point your LSP client to the `emmylua_ls` binary.

### Build from Source

```bash
git clone https://github.com/Pollux-Dev/gmod-glua-ls.git
cd gmod-glua-ls
cargo build --release
```

Binary location: `target/release/emmylua_ls` (or `emmylua_ls.exe` on Windows)

## Configuration

Create `.emmyrc.json` in your project root, configure as per [docs/config.md](./docs/config.md).

Note: Any existing LuaLS (`.luarc.json`) configuration will be used as fallback.

Here's a config that should be a good default for most:

```json
{
  "$schema": "https://raw.githubusercontent.com/Pollux12/gmod-glua-ls/refs/heads/main/crates/emmylua_code_analysis/resources/schema.json",
  "diagnostics": {
    "enable": true,
    "diagnosticInterval": 500,
    "severity": {
      "unused": "hint",
      "undefined-field": "information",
      "redundant-return": "hint",
      "redundant-return-value": "hint",
      "param-type-mismatch": "information",
      "missing-fields": "information",
      "assign-type-mismatch": "information",
      "return-type-mismatch": "information",
      "missing-parameter": "information",
      "cast-type-mismatch": "information",
      "need-check-nil": "hint"
    }
  }
}
```

See [docs/config.md](./docs/config.md) for all options.

## Annotations

Support for standard EmmyLua/LuaCATS annotations (`---@class`, `---@param`, etc.) plus GMod-specific additions such as `---@hook`.

See [docs/annotations/](./docs/annotations/) for the full reference.

## Credits

This is a hard fork of [EmmyLua Analyzer Rust](https://github.com/CppCXY/emmylua-analyzer-rust), maintained specifically for Garry's Mod GLua.
The original EmmyLua project does not support plugins, nor does it have any plan for any, making it difficult to fully adapt for Garry's Mod.
While LuaLS has plugin support, it was annoyingly slow to use. Many features here are based on my [LuaLS plugin](https://github.com/Pollux12/gmod-luals-addon).
