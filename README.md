# gmod-glua-ls

A fast, feature-rich language server for Garry's Mod Lua (GLua), written in Rust.

Built for developers who want accurate autocomplete, go-to-definition, and type checking without the slowdowns.

<!-- TODO: Add GIF showing autocomplete for GMod functions -->

## Features

### Made for Garry's Mod

- **Realm Inference** — Knows whether your code runs on client, server, or shared. Detects realm mismatches (like calling `ents.Create` in a `cl_` file) and shows inline realm hints.

- **Hook Autocomplete** — `hook.Add`, `hook.Run`, and `hook.Call` suggest available hooks. Add `---@hook` to your gamemode/plugin methods to register them as hook sources.

- **Scripted Class Support** — Full support for `ENT`, `SWEP`, `TOOL`, and custom scripted entities. Understands `DEFINE_BASECLASS`, `AccessorFunc`, and `NetworkVar` with synthesized getter/setter autocomplete. Supports wrapper functions (including local helpers) that call `NetworkVar` internally. Add `---@accessorfunc` to custom accessor generators for the same synthesis behavior.

- **Dynamic Field Inference** — Tracks fields dynamically set on Player, Entity, and other GMod objects to suppress false-positive "undefined field" warnings.

### Fast & Reliable

- Incremental analysis — instant feedback as you type
- Memory efficient — handles large codebases without issues
- Built with Rust for consistent performance

### LSP Features

- Autocomplete with snippets
- Go to definition / Find references
- Hover documentation
- Signature help
- Rename refactoring
- Diagnostics (errors, warnings, hints)
- Inlay hints (parameter names, types)
- Code actions and code lens

<!-- TODO: Add screenshot of inlay hints and diagnostics -->

## Installation

### VS Code

The recommended way is to use the GLua extension (coming soon). You can also configure the [EmmyLua extension](https://marketplace.visualstudio.com/items?itemName=tangzx.emmylua) to use this language server binary.

### Neovim

Using **lspconfig**:

```lua
require('lspconfig').emmylua_ls.setup({
    cmd = { 'emmylua_ls' },
    settings = {
        Lua = {
            gmod = { enabled = true }
        }
    }
})
```

### Other Editors

Any LSP-compatible editor works. Point your LSP client to the `emmylua_ls` binary.

### Build from Source

```bash
git clone https://github.com/Pollux-Dev/gmod-glua-ls.git
cd gmod-glua-ls
cargo build --release -p emmylua_ls
```

Binary location: `target/release/emmylua_ls` (or `emmylua_ls.exe` on Windows)

## Configuration

Create `.emmyrc.json` in your project root:

```json
{
  "$schema": "https://raw.githubusercontent.com/Pollux-Dev/gmod-glua-ls/main/crates/emmylua_code_analysis/resources/schema.json",
  "gmod": {
    "enabled": true,
    "defaultRealm": "shared"
  },
  "workspace": {
    "library": ["./lua/glua-api"]
  }
}
```

See [docs/config.md](./docs/config.md) for all options.

## Annotations

We support EmmyLua/LuaCATS annotations (`---@class`, `---@param`, etc.) plus GMod-specific additions:

```lua
---@realm server|client|shared
---Mark which realm this file/function belongs to

---@hook [HookName]
---Register a method as a hook handler (for gamemodes/plugins)
```

See [docs/annotations/](./docs/annotations/) for the full reference.

## About This Fork

This is a hard fork of [EmmyLua Analyzer Rust](https://github.com/CppCXY/emmylua-analyzer-rust), maintained specifically for Garry's Mod development. While the original EmmyLua provides excellent Lua analysis, this fork adds GMod-specific features like realm detection, hook analysis, and scripted class support that generic Lua language servers cannot provide.

## License

MIT
