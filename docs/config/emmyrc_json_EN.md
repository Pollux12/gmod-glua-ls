<div align="center">

# 🔧 EmmyLua Configuration Guide

[中文版](./emmyrc_json_CN.md)

*Master all configuration options of EmmyLua Analyzer Rust for more efficient Lua development*

</div>

---


### 📁 Configuration Files

<table>
<tr>
<td width="50%">

#### 📄 **Main Configuration File**
- **`.emmyrc.json`**: Primary configuration file
- **Location**: Project root directory
- **Priority**: Highest
- **Format**: JSON Schema support

</td>
<td width="50%">

#### 🔄 **Compatibility Configuration**
- **`.luarc.json`**: Compatibility configuration file
- **Auto Conversion**: Converts to `.emmyrc.json` format
- **Override Rules**: Overridden by `.emmyrc.json`
- **Compatibility**: Partial feature support

</td>
</tr>
</table>

> **💡 Tip**: The `.emmyrc.json` configuration format is richer and more flexible. It's recommended to use this format for the best experience.

### 🛠️ Schema Support

For intelligent completion and validation of configuration files, you can add a schema reference to the configuration file:

```json
{
  "$schema": "https://raw.githubusercontent.com/EmmyLuaLs/emmylua-analyzer-rust/refs/heads/main/crates/emmylua_code_analysis/resources/schema.json"
}
```

---

## 📝 Complete Configuration Example

Below is a complete configuration file example containing all configuration options:

<details>
<summary><b>🔧 Click to expand complete configuration</b></summary>

```json
{
    "$schema": "https://raw.githubusercontent.com/EmmyLuaLs/emmylua-analyzer-rust/refs/heads/main/crates/emmylua_code_analysis/resources/schema.json",
    "codeAction": {
        "insertSpace": false
    },
    "codeLens": {
        "enable": true
    },
    "completion": {
        "enable": true,
        "autoRequire": true,
        "autoRequireFunction": "require",
        "autoRequireNamingConvention": "keep",
        "autoRequireSeparator": ".",
        "callSnippet": false,
        "postfix": "@",
        "baseFunctionIncludesName": true
    },
    "diagnostics": {
        "enable": true,
        "disable": [],
        "enables": [],
        "globals": [],
        "globalsRegex": [],
        "severity": {},
        "diagnosticInterval": 500
    },
    "doc": {
        "syntax": "md"
    },
    "documentColor": {
        "enable": true
    },
    "hover": {
        "enable": true
    },
    "hint": {
        "enable": true,
        "paramHint": true,
        "indexHint": true,
        "localHint": true,
        "overrideHint": true,
        "metaCallHint": true
    },
    "inlineValues": {
        "enable": true
    },
    "references": {
        "enable": true,
        "fuzzySearch": true,
        "shortStringSearch": false
    },
    "reformat": {
        "externalTool": null,
        "externalToolRangeFormat": null,
        "useDiff": false
    },
    "resource": {
        "paths": []
    },
    "runtime": {
        "version": "LuaLatest",
        "requireLikeFunction": [],
        "frameworkVersions": [],
        "extensions": [],
        "requirePattern": [],
        "classDefaultCall": {
            "functionName": "",
            "forceNonColon": false,
            "forceReturnSelf": false
        },
        "nonstandardSymbol": [],
        "special": {}
    },
    "gmod": {
        "enabled": true,
        "defaultRealm": "shared",
        "scriptedClassScopes": {
            "include": [
                "entities/**",
                "weapons/**",
                "effects/**",
                "weapons/gmod_tool/stools/**"
            ],
            "exclude": []
        },
        "hookMappings": {
            "methodToHook": {},
            "emitterToHook": {}
        }
    },
    "semanticTokens": {
        "enable": true
    },
    "signature": {
        "detailSignatureHelper": true
    },
    "strict": {
        "requirePath": false,
        "typeCall": false,
        "arrayIndex": true,
        "metaOverrideFileDefine": true,
        "docBaseConstMatchBaseType": true
    },
    "workspace": {
        "ignoreDir": [],
        "ignoreGlobs": [],
        "library": [],
        "workspaceRoots": [],
        "preloadFileSize": 0,
        "encoding": "utf-8",
        "moduleMap": [],
        "reindexDuration": 5000,
        "enableReindex": false
    }
}
```

</details>

---

## 🎯 Configuration Details

### 💡 completion - Code Completion

<div align="center">

#### Intelligent completion configuration for enhanced coding efficiency

</div>

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`enable`** | `boolean` | `true` | 🔧 Enable/disable code completion feature |
| **`autoRequire`** | `boolean` | `true` | 📦 Auto-complete require statements |
| **`autoRequireFunction`** | `string` | `"require"` | ⚡ Function name for auto-completion |
| **`autoRequireNamingConvention`** | `string` | `"keep"` | 🏷️ Naming convention conversion method |
| **`autoRequireSeparator`** | `string` | `"."` | 🔗 Auto-require path separator |
| **`callSnippet`** | `boolean` | `false` | 🎪 Enable function call snippets |
| **`postfix`** | `string` | `"@"` | 🔧 Postfix completion trigger symbol |
| **`baseFunctionIncludesName`** | `boolean` | `true` | 📝 Include function name in base function completion |

#### 🏷️ Naming Convention Options

<table>
<tr>
<td width="25%">

**`keep`**
Keep original

</td>
<td width="25%">

**`camel-case`**
Camel case

</td>
<td width="25%">

**`snake-case`**
Snake case

</td>
<td width="25%">

**`pascal-case`**
Pascal case

</td>
</tr>
</table>

---

### 🎯 codeAction - Code Actions

<div align="center">

#### Code quick fixes and refactoring operation configurations

</div>

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`insertSpace`** | `boolean` | `false` | 🔧 Insert space when adding `@diagnostic disable-next-line` after `---` comments |

---

### 📄 doc - Documentation Syntax

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`syntax`** | `string` | `"md"` | 📝 Documentation comment syntax type |

#### 📚 Supported Documentation Syntax

<table>
<tr>
<td width="50%">

**`md`**
Markdown syntax

</td>
<td width="50%">

**`myst`**
MyST syntax

</td>
</tr>
</table>

---

### 🎨 documentColor - Document Color

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`enable`** | `boolean` | `true` | 🌈 Enable/disable color display functionality in documents |

---

### 🔧 reformat - Code Formatting

see [External Formatter Options](../external_format/external_formatter_options_CN.md)

---

### 📊 inlineValues - Inline Values

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`enable`** | `boolean` | `true` | 🔍 Enable/disable inline value display during debugging |

---

### 📝 signature - Function Signature

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`detailSignatureHelper`** | `boolean` | `false` | 📊 Show detailed function signature help (currently ineffective) |

---

### 🔍 diagnostics - Code Diagnostics

<div align="center">

#### Powerful static analysis and error detection system

</div>

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`disable`** | `string[]` | `[]` | ❌ List of disabled diagnostic messages |
| **`globals`** | `string[]` | `[]` | 🌐 Global variable whitelist |
| **`globalsRegex`** | `string[]` | `[]` | 🔤 Global variable regex patterns |
| **`severity`** | `object` | `{}` | ⚠️ Diagnostic message severity configuration |
| **`enables`** | `string[]` | `[]` | ✅ List of enabled diagnostic messages |

#### 🎯 Severity Levels

<table>
<tr>
<td width="25%">

**`error`**
🔴 Error

</td>
<td width="25%">

**`warning`**
🟡 Warning

</td>
<td width="25%">

**`information`**
🔵 Information

</td>
<td width="25%">

**`hint`**
💡 Hint

</td>
</tr>
</table>

#### 📋 Common Diagnostic Configuration Example

```json
{
  "diagnostics": {
    "disable": ["undefined-global"],
    "severity": {
      "undefined-global": "warning",
      "unused": "hint"
    },
    "enables": ["undefined-field"]
  }
}
```

### Available Diagnostics List

| Diagnostic Message | Description | Default Category |
|-----------|------|------|
| **`syntax-error`** | Syntax errors | 🔴 Error |
| **`doc-syntax-error`** | Documentation syntax errors | 🔴 Error |
| **`type-not-found`** | Type not found | 🟡 Warning |
| **`missing-return`** | Missing return statement | 🟡 Warning |
| **`param-type-not-match`** | Parameter type mismatch | 🟡 Warning |
| **`missing-parameter`** | Missing parameter | 🟡 Warning |
| **`redundant-parameter`** | Redundant parameter | 🟡 Warning |
| **`unreachable-code`** | Unreachable code | 💡 Hint |
| **`unused`** | Unused variable/function | 💡 Hint |
| **`undefined-global`** | Undefined global variable | 🔴 Error |
| **`deprecated`** | Deprecated feature | 🔵 Hint |
| **`access-invisible`** | Access to invisible member | 🟡 Warning |
| **`discard-returns`** | Discarded return values | 🟡 Warning |
| **`undefined-field`** | Undefined field | 🟡 Warning |
| **`local-const-reassign`** | Local constant reassignment | 🔴 Error |
| **`iter-variable-reassign`** | Iterator variable reassignment | 🟡 Warning |
| **`duplicate-type`** | Duplicate type definition | 🟡 Warning |
| **`redefined-local`** | Redefined local variable | 💡 Hint |
| **`redefined-label`** | Redefined label | 🟡 Warning |
| **`code-style-check`** | Code style check | 🟡 Warning |
| **`need-check-nil`** | Need nil check | 🟡 Warning |
| **`await-in-sync`** | Using await in synchronous code | 🟡 Warning |
| **`annotation-usage-error`** | Annotation usage error | 🔴 Error |
| **`return-type-mismatch`** | Return type mismatch | 🟡 Warning |
| **`missing-return-value`** | Missing return value | 🟡 Warning |
| **`redundant-return-value`** | Redundant return value | 🟡 Warning |
| **`undefined-doc-param`** | Undefined parameter in documentation | 🟡 Warning |
| **`duplicate-doc-field`** | Duplicate documentation field | 🟡 Warning |
| **`missing-fields`** | Missing fields | 🟡 Warning |
| **`inject-field`** | Inject field | 🟡 Warning |
| **`circle-doc-class`** | Circular documentation class inheritance | 🟡 Warning |
| **`incomplete-signature-doc`** | Incomplete signature documentation | 🟡 Warning |
| **`missing-global-doc`** | Missing global variable documentation | 🟡 Warning |
| **`assign-type-mismatch`** | Assignment type mismatch | 🟡 Warning |
| **`duplicate-require`** | Duplicate require | 💡 Hint |
| **`non-literal-expressions-in-assert`** | Non-literal expressions in assert | 🟡 Warning |
| **`unbalanced-assignments`** | Unbalanced assignments | 🟡 Warning |
| **`unnecessary-assert`** | Unnecessary assert | 🟡 Warning |
| **`unnecessary-if`** | Unnecessary if statement | 🟡 Warning |
| **`duplicate-set-field`** | Duplicate field assignment | 🟡 Warning |
| **`duplicate-index`** | Duplicate index | 🟡 Warning |
| **`generic-constraint-mismatch`** | Generic constraint mismatch | 🟡 Warning |

---

### 💡 hint - Inline Hints

<div align="center">

#### Intelligent inline hint system for viewing type information without mouse hover

</div>

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`enable`** | `boolean` | `true` | 🔧 Enable/disable inline hints |
| **`paramHint`** | `boolean` | `true` | 🏷️ Show function parameter hints |
| **`indexHint`** | `boolean` | `true` | 📊 Show cross-line index expression hints |
| **`localHint`** | `boolean` | `true` | 📍 Show local variable type hints |
| **`overrideHint`** | `boolean` | `true` | 🔄 Show method override hints |
| **`metaCallHint`** | `boolean` | `true` | 🎭 Show metatable `__call` invocation hints |

---

### ⚙️ runtime - Runtime Environment

<div align="center">

#### Configure Lua runtime environment and version features

</div>

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`version`** | `string` | `"LuaLatest"` | 🚀 Lua version selection |
| **`requireLikeFunction`** | `string[]` | `[]` | 📦 List of require-like functions |
| **`frameworkVersions`** | `string[]` | `[]` | 🎯 Framework version identifiers |
| **`extensions`** | `string[]` | `[]` | 📄 Supported file extensions |
| **`requirePattern`** | `string[]` | `[]` | 🔍 Require pattern matching rules |
| **`classDefaultCall`** | `object` | `{}` | 🏗️ Class default call configuration |
| **`nonstandardSymbol`** | `string[]` | `[]` | 🔧 Non-standard symbol list |
| **`special`** | `object` | `{}` | ✨ Special symbol configuration |

#### 🚀 Supported Lua Versions

<table>
<tr>
<td width="16.6%">

**`Lua5.1`**
Classic version

</td>
<td width="16.6%">

**`Lua5.2`**
Enhanced features

</td>
<td width="16.6%">

**`Lua5.3`**
Integer support

</td>
<td width="16.6%">

**`Lua5.4`**
Latest features

</td>
<td width="16.6%">

**`LuaJIT`**
High performance

</td>
<td width="16.6%">

**`LuaLatest`**
Latest feature set

</td>
</tr>
</table>

#### 📋 Runtime Configuration Example

```json
{
  "runtime": {
    "version": "LuaLatest",
    "requireLikeFunction": ["import", "load", "dofile"],
    "frameworkVersions": ["love2d", "openresty", "nginx"],
    "extensions": [".lua", ".lua.txt", ".luau"],
    "requirePattern": ["?.lua", "?/init.lua", "lib/?.lua"],
    "classDefaultCall": {
      "functionName": "new",
      "forceNonColon": false,
      "forceReturnSelf": true
    },
    "nonstandardSymbol": ["continue"],
    "special": {
      "errorf":"error"
    }
  }
}
```

---

### 🎮 gmod - Garry's Mod Options

<div align="center">

#### Garry's Mod settings for realm, hooks, and scripted-class analysis

</div>

Defaults are aligned with the LuaLS addon where supported, especially scripted class scope paths.

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`enabled`** | `boolean` | `true` | 🔧 Enable/disable GMod-specific analysis features |
| **`defaultRealm`** | `string` | `"shared"` | 🌐 Fallback realm when no explicit realm signal is found |
| **`scriptedClassScopes.include`** | `string[]` | `["entities/**","weapons/**","effects/**","weapons/gmod_tool/stools/**"]` | 📦 Glob patterns where scripted-class call extraction is allowed |
| **`scriptedClassScopes.exclude`** | `string[]` | `[]` | 🚫 Glob patterns to skip scripted-class extraction |
| **`hookMappings.methodToHook`** | `object` | `{}` | 🪝 Map custom methods (for example `PLUGIN:PlayerSpawn`) to hook names |
| **`hookMappings.emitterToHook`** | `object` | `{}` | 📣 Map custom emitters (for example `MyHooks.Emit`) to hook names |
| **`hookMappings.methodPrefixes`** | `string[]` | `[]` | 🧭 Prefixes that should auto-map `Prefix:Method` as hook `Method` (for example `PLUGIN`) |
| **`detectRealmFromFilename`** | `boolean \| null` | `null` | 🧭 Optional toggle for filename-based realm detection |
| **`detectRealmFromCalls`** | `boolean \| null` | `null` | 🧠 Optional toggle for call-site-based realm detection |

#### 📋 GMod Configuration Example

```json
{
  "gmod": {
    "enabled": true,
    "defaultRealm": "shared",
    "scriptedClassScopes": {
      "include": [
        "entities/**",
        "weapons/**",
        "effects/**",
        "weapons/gmod_tool/stools/**"
      ],
      "exclude": []
    },
    "hookMappings": {
      "methodToHook": {},
      "emitterToHook": {},
      "methodPrefixes": []
    },
    "detectRealmFromFilename": true
  }
}
```

#### 🪝 Automatic Hook Detection Rules

- `hook.Add("Name", ...)`, `hook.Run("Name")`, and `hook.Call("Name", ...)` are detected automatically when names are static strings.
- `GM:MethodName` and `GAMEMODE:MethodName` are automatically treated as hooks.
- `---@hook` on method functions enables automatic hook registration from annotations:
  - `---@hook` + `function PLUGIN:PlayerSpawn()` -> hook name `PlayerSpawn`
  - `---@hook CustomName` + `function PLUGIN:OnX()` -> hook name `CustomName`
- Hook names discovered from `hook.Add`, method hooks, and `---@hook` are available in autocomplete for `hook.Run`, `hook.Call`, and `hook.Add` first-string arguments.
- Hook completion details include inferred callback arg names when available.
- `hookMappings` is optional for overrides and framework-specific conventions:
  - `methodToHook` for explicit remaps
  - `methodPrefixes` for prefix-wide auto behavior (Helix-style `PLUGIN:*`, etc.)
  - `emitterToHook` for custom emitter APIs

#### 🧩 Plugin Folder Detection (Entity-like Scope Behavior)

If your framework has plugin folders (for example `plugins` or `gamemode/plugins`) and you want scripted-class extraction (`DEFINE_BASECLASS`, `AccessorFunc`, `NetworkVar`) there too, include those folders in `scriptedClassScopes.include`.

```json
{
  "gmod": {
    "scriptedClassScopes": {
      "include": [
        "entities/**",
        "weapons/**",
        "effects/**",
        "plugins/**",
        "gamemode/plugins/**",
        "gamemode/modules/**"
      ],
      "exclude": [
        "**/tests/**",
        "**/test/**"
      ]
    }
  }
}
```

> Matching accepts normalized full paths, `lua/...` relative paths, and path suffixes (for example `entities/**` matches `addons/x/gamemode/entities/...`), so these patterns work in nested addon/gamemode layouts.

#### 📚 Full Guide

For end-to-end setup (server launch, recommended `.emmyrc.json`, plugin-folder recipes, and troubleshooting), see:  
[`docs/config/gmod_setup_EN.md`](./gmod_setup_EN.md)

---

### 🏗️ workspace - Workspace Configuration

<div align="center">

#### Workspace and project structure configuration, supporting both relative and absolute paths

</div>

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`ignoreDir`** | `string[]` | `[]` | 📁 List of directories to ignore |
| **`ignoreGlobs`** | `string[]` | `[]` | 🔍 Glob pattern-based file ignore rules |
| **`library`** | `string[]` | `[]` | 📚 Library directory paths |
| **`workspaceRoots`** | `string[]` | `[]` | 🏠 Workspace root directory list |
| **`encoding`** | `string` | `"utf-8"` | 🔤 File encoding format |
| **`moduleMap`** | `object[]` | `[]` | 🗺️ Module path mapping rules |
| **`reindexDuration`** | `number` | `5000` | ⏱️ Reindexing time interval (milliseconds) |

#### 🗺️ Module Mapping Configuration

Module mapping is used to convert one module path to another, supporting regular expressions:

```json
{
  "workspace": {
    "moduleMap": [
      {
        "pattern": "^lib(.*)$",
        "replace": "script$1"
      }
    ]
  }
}
```

#### 📋 Workspace Configuration Example

```json
{
  "workspace": {
    "ignoreDir": ["build", "dist", "node_modules"],
    "ignoreGlobs": ["*.log", "*.tmp", "test_*"],
    "library": ["/usr/local/lib/lua", "./libs"],
    "workspaceRoots": ["Assets/Scripts/Lua"],
    "encoding": "utf-8",
    "reindexDuration": 3000
  }
}
```

---

### 📁 resource - Resource Paths

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`paths`** | `string[]` | `[]` | 🎯 List of resource file root directories |

> **💡 Purpose**: Configuring resource directories allows EmmyLua to properly provide file path completion and navigation functionality.

---

### 👁️ codeLens - Code Lens

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`enable`** | `boolean` | `true` | 🔍 Enable/disable CodeLens functionality |

---

### 🔒 strict - Strict Mode

<div align="center">

#### Strict mode configuration to control the strictness of type checking and code analysis

</div>

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`requirePath`** | `boolean` | `false` | 📍 Require path strict mode |
| **`typeCall`** | `boolean` | `false` | 🎯 Type call strict mode |
| **`arrayIndex`** | `boolean` | `false` | 📊 Array index strict mode |
| **`metaOverrideFileDefine`** | `boolean` | `true` | 🔄 Meta definitions override file definitions |

#### 🎯 Strict Mode Explanation

<table>
<tr>
<td width="50%">

**🔒 When Strict Mode is Enabled**
- **Require Path**: Must start from specified root directories
- **Type Call**: Manual overload definitions required
- **Array Index**: Strict adherence to indexing rules
- **Meta Definitions**: Override definitions in files

</td>
<td width="50%">

**🔓 When Strict Mode is Disabled**
- **Require Path**: Flexible path resolution
- **Type Call**: Returns self type
- **Array Index**: Lenient indexing checks
- **Meta Definitions**: Behavior similar to `luals`

</td>
</tr>
</table>

---

### 👁️ hover - Hover Information

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`enable`** | `boolean` | `true` | 🖱️ Enable/disable mouse hover information |

---

### 🔗 references - Reference Finding

| Configuration | Type | Default | Description |
|--------|------|--------|------|
| **`enable`** | `boolean` | `true` | 🔍 Enable/disable reference finding functionality |
| **`fuzzySearch`** | `boolean` | `true` | 🎯 Enable fuzzy search |
| **`shortStringSearch`** | `boolean` | `false` | 🔤 Enable short string search |

---


### 📚 Related Resources

<div align="center">

[![GitHub](https://img.shields.io/badge/GitHub-EmmyLuaLs/emmylua--analyzer--rust-blue?style=for-the-badge&logo=github)](https://github.com/EmmyLuaLs/emmylua-analyzer-rust)
[![Documentation](https://img.shields.io/badge/Documentation-Complete%20Configuration%20Guide-green?style=for-the-badge&logo=gitbook)](../../README.md)
[![Issues](https://img.shields.io/badge/Issue%20Reporting-GitHub%20Issues-red?style=for-the-badge&logo=github)](https://github.com/EmmyLuaLs/emmylua-analyzer-rust/issues)

</div>

---

### 🎉 Getting Started

1. **Create Configuration File**: Create `.emmyrc.json` in the project root directory
2. **Add Schema**: Copy the schema URL above to get intelligent hints
3. **Configure Gradually**: Add configuration items step by step according to project requirements
4. **Test and Validate**: Save configuration and test language server functionality

> **💡 Tip**: It's recommended to start with basic configuration and gradually add advanced features to better understand the purpose of each configuration item.

[⬆ Back to Top](#-emmylua-configuration-guide)

</div>
