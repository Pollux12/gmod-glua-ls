# Garry's Mod Specific Setup

This version is designed to work with Garry's Mod, I'm not testing with regular Lua.
- `gmod.enabled` defaults to `true`
- `gmod.defaultRealm` defaults to `"shared"`
- `gmod.scriptedClassScopes.include` defaults to the LuaLS plugin parity scope set:
  - `entities/**`
  - `weapons/**`
  - `effects/**`
  - `weapons/gmod_tool/stools/**`
- GMod analysis is active out of the box

---

## 1) Build and run

From repository root:

```bash
cargo build --release -p emmylua_ls
```

Binary path:

- Windows: `target\release\emmylua_ls.exe`
- Linux/macOS: `target/release/emmylua_ls`

You can run manually for quick verification:

```bash
target\release\emmylua_ls.exe
```

---

## 2) Editor setup

Use any LSP client and point command to the built `emmylua_ls` binary.

### VS Code

- Install EmmyLua extension.
- Configure it to launch this fork's `emmylua_ls` binary (custom executable path in your extension/LSP setup).

### Neovim (example)

```lua
vim.lsp.config("emmylua_ls", {
  cmd = { "C:/path/to/emmylua-analyzer-rust/target/release/emmylua_ls.exe" },
})
vim.lsp.enable("emmylua_ls")
```

---

## 3) Recommended `.emmyrc.json` for GMod

Create `.emmyrc.json` in workspace root:

```json
{
  "$schema": "https://raw.githubusercontent.com/EmmyLuaLs/emmylua-analyzer-rust/refs/heads/main/crates/emmylua_code_analysis/resources/schema.json",
  "gmod": {
    "enabled": true,
    "defaultRealm": "shared",
    "detectRealmFromFilename": true,
    "detectRealmFromCalls": true,
    "scriptedClassScopes": {
      "include": [
        "entities/**",
        "weapons/**",
        "effects/**",
        "weapons/gmod_tool/stools/**"
      ]
    },
    "hookMappings": {
      "methodToHook": {},
      "emitterToHook": {},
      "methodPrefixes": []
    }
  },
  "workspace": {
    "library": [
      "./glua-api-snippets/output"
    ]
  }
}
```

---

## 4) Plugin-folder detection and usage

This fork's scripted-class extraction (for `DEFINE_BASECLASS`, `AccessorFunc`, `NetworkVar`) is scope-filtered by `gmod.scriptedClassScopes`.

To treat plugin systems like entity folders, include plugin paths in `scriptedClassScopes.include`:

- `weapons/gmod_tool/stools/**` (already part of the default plugin parity set for TOOL)
- `plugins/**`
- `gamemode/plugins/**`
- `gamemode/modules/**`

This makes plugin files participate in the same scripted-class extraction pipeline used for entity-style authoring.
Patterns are evaluated against full paths, `lua/...` relative paths, and suffix paths, so folder-style defaults like `entities/**` also match nested paths such as `addons/x/gamemode/entities/...`.

If your framework uses additional method prefixes beyond defaults, configure them in `hookMappings.methodPrefixes`:

```json
{
  "gmod": {
    "scriptedClassScopes": {
      "include": [
        "entities/**",
        "weapons/**",
        "effects/**",
        "weapons/gmod_tool/stools/**",
        "plugins/**",
        "gamemode/plugins/**"
      ]
    },
    "hookMappings": {
      "methodPrefixes": ["MYFRAMEWORK"]
    }
  }
}
```

Typical layout:

- `gamemode/plugins/vehicles/sh_plugin.lua`
- `gamemode/plugins/doors/sv_plugin.lua`

Expected behavior in those plugin folders:

- `PLUGIN` binds to inferred class `<plugin-folder-name>` (for example `vehicles`).
- In scoped plugin files, the inferred plugin class keeps `PLUGIN` ancestry and also inherits from `GM` so standard gamemode hook docs/signatures can flow.
- `PLUGIN:Method` is treated as a hook method by default.
- `---@hook` on methods contributes hook names used by hook-name completion.

---

## 5) Hook registration behavior

The server supports automatic hook registration without requiring manual mapping for every method:

- `GM:Method`, `GAMEMODE:Method`, `PLUGIN:Method`, and `SANDBOX:Method` are auto-treated as hooks.
- `---@hook` on a method registers it as a hook source:
  - `---@hook` uses the method name.
  - `---@hook CustomHookName` uses `CustomHookName`.
- `hook.Run(...)`/`hook.Call(...)` and `hook.Add(...)` are parsed as hook call/emit sites when hook names are static.
- Hook names discovered from `hook.Add`, method hooks, and `---@hook` are offered in autocomplete for `hook.Run("...")`, `hook.Call("...")`, and `hook.Add("...")`.
- Hook completion details include inferred callback arg names when they are available from method definitions or inline `hook.Add` closures.
- `hookMappings.methodPrefixes` allows additional framework-style prefixes (beyond built-ins) to behave like `GM:`.

Use `hookMappings.methodToHook` only for explicit overrides when inferred naming is not enough.

---

## 6) What gets inferred today

- Realm hints from filename patterns and dependency/call signals
- Hook sites:
  - `hook.Add(...)`
  - `hook.Run(...)` / `hook.Call(...)`
  - `GM:*`, `GAMEMODE:*`, `PLUGIN:*`, and `SANDBOX:*` methods
  - `---@hook` on methods
  - `methodPrefixes`-configured custom prefixes
  - custom mappings from `gmod.hookMappings`
- System metadata:
  - `util.AddNetworkString`, `net.Start`, `net.Receive`
  - `concommand.Add`
  - `CreateConVar`, `CreateClientConVar`
  - `timer.Create`, `timer.Simple`

---

## 7) Troubleshooting

- No GMod diagnostics/completion?
  - Check the workspace is using this fork's binary.
  - Confirm `.emmyrc.json` is loaded from workspace root.
  - Ensure `gmod.enabled` is not overridden to `false`.
- Plugin folders not being recognized?
  - Add them to `gmod.scriptedClassScopes.include`.
  - Use forward-slash glob patterns as in examples above.
- Missing API symbols?
  - Add generated annotation output directory to `workspace.library`.
