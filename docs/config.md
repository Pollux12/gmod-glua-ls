# Configuration

Configuration is done via `.emmyrc.json` (or `.luarc.json` for compatibility) in your workspace root.

## Quick Start

This config should be good for most, feel free to enable/disable diagnostics or change severity levels depending on how strict you want the checks to be.

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

---

## `codeAction`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `insertSpace` | `boolean` | `false` | Add space after `---` when inserting `@diagnostic disable-next-line` |

---

## `codeLens`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | `boolean` | `true` | Enable code lens |

---

## `completion`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | `boolean` | `true` | Enable autocomplete |
| `autoRequire` | `boolean` | `true` | Auto-insert require when completing items from other modules |
| `autoRequireFunction` | `string` | `"require"` | Function name used for auto-require |
| `autoRequireNamingConvention` | `string` | `"keep"` | Naming convention: `"keep"`, `"snake-case"`, `"pascal-case"`, `"camel-case"`, `"keep-class"` |
| `autoRequireSeparator` | `string` | `"."` | Separator used in auto-require paths |
| `callSnippet` | `boolean` | `false` | Use call snippets in completions |
| `postfix` | `string` | `"@"` | Symbol to trigger postfix completion: `@`, `.`, `:` |
| `baseFunctionIncludesName` | `boolean` | `true` | Include function name in base completion |

---

## `diagnostics`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | `boolean` | `true` | Enable diagnostics |
| `disable` | `string[]` | `[]` | List of diagnostic codes to disable |
| `enables` | `string[]` | `[]` | List of diagnostic codes to explicitly enable |
| `globals` | `string[]` | `[]` | Global variable whitelist |
| `globalsRegex` | `string[]` | `[]` | Regex patterns for global variable whitelist |
| `severity` | `object` | `{}` | Map of diagnostic codes to severity levels |
| `diagnosticInterval` | `integer` | `500` | Delay in ms between file changes and diagnostics scan |

### Severity Levels

- `"error"` - Red error indicator
- `"warning"` - Yellow warning indicator
- `"information"` - Blue information indicator
- `"hint"` - Subtle hint indicator

### Diagnostic Codes

| Code | Default | Severity | Description |
|------|---------|----------|-------------|
| `syntax-error` | On | Error | Syntax errors |
| `doc-syntax-error` | On | Error | Documentation annotation syntax errors |
| `type-not-found` | On | Warning | Referenced type not found |
| `missing-return` | **Off** | Warning | Function missing return statement |
| `param-type-mismatch` | On | Warning | Parameter type doesn't match |
| `missing-parameter` | On | Warning | Missing required parameter |
| `redundant-parameter` | On | Warning | Extra parameter passed |
| `unreachable-code` | On | Hint | Code can never be executed |
| `unused` | On | Hint | Unused variable/function |
| `unused-self` | **Off** | Hint | Unused implicit self parameter |
| `undefined-global` | On | Error | Undefined global variable |
| `deprecated` | On | Hint | Use of deprecated function/field |
| `access-invisible` | On | Warning | Access to private/protected member |
| `discard-returns` | On | Warning | Return value not used (for `@nodiscard` functions) |
| `undefined-field` | On | Warning | Field doesn't exist on type |
| `local-const-reassign` | On | Error | Reassigning a local const |
| `duplicate-type` | **Off** | Warning | Type defined multiple times |
| `redefined-local` | On | Hint | Local variable redefined |
| `redefined-label` | On | Warning | Label redefined |
| `code-style-check` | **Off** | Warning | Code style violations |
| `need-check-nil` | On | Hint | Potential nil dereference |
| `await-in-sync` | On | Warning | Using await in synchronous function |
| `annotation-usage-error` | On | Error | Incorrect annotation usage |
| `return-type-mismatch` | **Off** | Warning | Return type doesn't match |
| `missing-return-value` | On | Warning | Missing return value |
| `redundant-return-value` | **Off** | Warning | Extra return value |
| `undefined-doc-param` | On | Warning | Documented parameter doesn't exist |
| `duplicate-doc-field` | On | Warning | Documented field defined multiple times |
| `unknown-doc-tag` | **Off** | Warning | Unknown documentation annotation |
| `missing-fields` | On | Warning | Required fields not set |
| `inject-field` | **Off** | Warning | Field injected into type |
| `circle-doc-class` | On | Warning | Circular class inheritance |
| `incomplete-signature-doc` | **Off** | Warning | Missing documentation for parameters/returns |
| `missing-global-doc` | **Off** | Warning | Global missing documentation |
| `assign-type-mismatch` | On | Warning | Assignment type doesn't match |
| `duplicate-require` | On | Hint | Module required multiple times |
| `non-literal-expressions-in-assert` | **Off** | Warning | Non-literal in assert |
| `unbalanced-assignments` | On | Warning | Unequal values in assignment |
| `unnecessary-assert` | **Off** | Warning | Assert that always passes |
| `unnecessary-if` | **Off** | Warning | If statement always true/false |
| `duplicate-set-field` | **Off** | Warning | Field set multiple times |
| `duplicate-index` | On | Warning | Index used multiple times |
| `generic-constraint-mismatch` | On | Information | Generic constraint violation |
| `cast-type-mismatch` | On | Warning | Cast type incompatible |
| `require-module-not-visible` | On | Warning | Required module not accessible |
| `enum-value-mismatch` | On | Warning | Value doesn't match enum |
| `preferred-local-alias` | On | Hint | Prefer local alias over global |
| `read-only` | On | Warning | Writing to read-only value |
| `global-in-non-module` | **Off** | Warning | Global defined in non-module scope |
| `attribute-param-type-mismatch` | On | Warning | Attribute parameter type mismatch |
| `attribute-missing-parameter` | On | Warning | Missing attribute parameter |
| `attribute-redundant-parameter` | On | Warning | Extra attribute parameter |
| `invert-if` | **Off** | Warning | If can be inverted for clarity |
| `call-non-callable` | **Off** | Warning | Calling non-callable value |
| `gmod-invalid-hook-name` | On | Warning | Invalid hook name |
| `gmod-realm-misuse` | On | Warning | Client/server API used in wrong realm |
| `gmod-realm-misuse-risky` | On | Hint | Risky realm usage detected |
| `gmod-unknown-net-message` | On | Warning | Unknown net message identifier |
| `gmod-duplicate-system-registration` | On | Hint | Duplicate registration (concommand, net, timer, etc.) |

**Note:** Diagnostics marked **Off** are disabled by default and must be added to `enables` to activate.

---

## `doc`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `syntax` | `string` | `"md"` | Documentation syntax: `"md"`, `"myst"`, `"rst"`, `"none"` |
| `knownTags` | `string[]` | `[]` | List of known custom tags |
| `privateName` | `string[]` | `[]` | Field name patterns treated as private (e.g., `m_*`) |
| `rstDefaultRole` | `string \| null` | `null` | Default role for RST syntax |
| `rstPrimaryDomain` | `string \| null` | `null` | Primary domain for RST syntax |

---

## `documentColor`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | `boolean` | `true` | Enable color picker in editor |

---

## `format`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `externalTool` | `object \| null` | `null` | External formatter configuration |
| `externalToolRangeFormat` | `object \| null` | `null` | External formatter for range formatting |
| `useDiff` | `boolean` | `false` | Use diff algorithm for formatting |

### External Tool Format

```json
{
  "program": "stylua",
  "args": ["--stdin-filepath", "$FILENAME"],
  "timeout": 5000
}
```

---

## `gmod`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enabled` | `boolean` | `true` | Enable GMod-specific analysis |
| `defaultRealm` | `string` | `"shared"` | Default realm: `"client"`, `"server"`, `"shared"`, `"menu"` |
| `detectRealmFromFilename` | `boolean \| null` | `true` | Detect realm from filename prefixes (`cl_`, `sv_`, `sh_`) |
| `detectRealmFromCalls` | `boolean \| null` | `true` | Detect realm from API usage |
| `inferDynamicFields` | `boolean` | `true` | Track dynamic fields on GMod objects |
| `scriptedClassScopes.include` | `string[]` | `["entities/**", "weapons/**", "effects/**", "weapons/gmod_tool/stools/**"]` | Glob patterns for scripted class extraction |
| `scriptedClassScopes.exclude` | `string[]` | `[]` | Patterns to exclude from scripted class extraction |
| `hookMappings.methodToHook` | `object` | `{}` | Map methods to hook names |
| `hookMappings.emitterToHook` | `object` | `{}` | Map custom emitters to hook names |
| `hookMappings.methodPrefixes` | `string[]` | `[]` | Additional prefixes for hook auto-detection |

### Scripted Class Analysis

Scripted class analysis runs on files matched by `gmod.scriptedClassScopes` and synthesizes members for common Garry's Mod patterns:

- `AccessorFunc(target, "m_Field", "Name", ...)` synthesizes `GetName()` and `SetName(value)`
- `NetworkVar(...)` / `NetworkVarElement(...)` synthesize getter/setter pairs for declared data table vars
- Wrapper functions that call `NetworkVar` are detected, including local helper functions declared inside `SetupDataTables`
- `---@accessorfunc` extends accessor synthesis to custom generator functions (works on any class)

```lua
function ENT:RegisterVar(varType, slot, name)
  -- Method wrapper
  self:NetworkVar(varType, slot, name)
end

---@accessorfunc 2
function ENT:MakeAccessor(prefix, name)
  -- Custom accessor generator (name is arg #2)
end

function ENT:SetupDataTables()
  -- Direct NetworkVar call
  self:NetworkVar("Float", 0, "Speed")

  -- Method wrapper call
  self:RegisterVar("Int", 1, "Ammo")

  -- Local function wrapper call
  local function addBool(slot, name)
    self:NetworkVar("Bool", slot, name)
  end
  addBool(2, "IsReady")

  -- Custom accessor synthesis via @accessorfunc
  self:MakeAccessor("Net", "OwnerName")
end
```

---

## `hint`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | `boolean` | `true` | Enable inlay hints |
| `paramHint` | `boolean` | `true` | Show parameter names in function calls |
| `localHint` | `boolean` | `true` | Show types of local variables |
| `indexHint` | `boolean` | `true` | Show named array indexes |
| `overrideHint` | `boolean` | `true` | Show methods that override base class |
| `metaCallHint` | `boolean` | `true` | Show `__call` metatable hints |
| `enumParamHint` | `boolean` | `false` | Show enum names for literal values |

---

## `hover`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | `boolean` | `true` | Enable hover information |
| `customDetail` | `integer \| null` | `null` | Detail level 0-255, null = default |

---

## `inlineValues`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | `boolean` | `true` | Show inline values during debug |

---

## `references`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | `boolean` | `true` | Enable find references |
| `fuzzySearch` | `boolean` | `true` | Use fuzzy search when exact search fails |
| `shortStringSearch` | `boolean` | `false` | Search for references in strings |

---

## `resource`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `paths` | `string[]` | `[]` | Resource file root directories |

---

## `runtime`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `version` | `string` | `"LuaJIT"` | **Deprecated:** Always LuaJIT in this fork |
| `extensions` | `string[]` | `[]` | Additional file extensions (e.g., `.lua.txt`) |
| `requireLikeFunction` | `string[]` | `[]` | Functions that behave like `require` |
| `requirePattern` | `string[]` | `[]` | Require path patterns (e.g., `?.lua`, `?/init.lua`) |
| `nonstandardSymbol` | `string[]` | `["//", "/***/", "continue", "!=", "||", "&&", "!"]` | Non-standard Lua symbols to support |
| `frameworkVersions` | `string[]` | `[]` | Framework version identifiers |
| `special` | `object` | `{}` | Special symbol mappings |

### Non-Standard Symbols

Available: `//`, `/**/`, `` ` ``, `+=`, `-=`, `*=`, `/=`, `%=`, `^=`, `//=`, `|=`, `&=`, `<<=`, `>>=`, `||`, `&&`, `!`, `!=`, `continue`

Default: `["//", "/***/", "continue", "!=", "||", "&&", "!"]`

### Special Symbols

Map function names to special behaviors: `none`, `require`, `error`, `assert`, `type`, `setmetatable`

```json
{
  "special": {
    "myrequire": "require",
    "myassert": "assert"
  }
}
```

---

## `semanticTokens`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | `boolean` | `true` | Enable semantic tokens |
| `renderDocumentationMarkup` | `boolean` | `false` | Render Markdown/RST in semantic token documentation |

---

## `signature`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `detailSignatureHelper` | `boolean` | `true` | Enable signature help |

---

## `strict`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `requirePath` | `boolean` | `false` | Strict require path checking |
| `typeCall` | `boolean` | `false` | Strict type call checking |
| `arrayIndex` | `boolean` | `true` | Strict array index checking |
| `metaOverrideFileDefine` | `boolean` | `true` | Meta definitions override file definitions |
| `docBaseConstMatchBaseType` | `boolean` | `false` | Allow base constants to match base types |
| `requireExportGlobal` | `boolean` | `false` | Require `---@export global` for library visibility |

---

## `workspace`

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `library` | `array` | `[]` | Library paths (strings or objects with `path`, `ignoreDir`, `ignoreGlobs`) |
| `workspaceRoots` | `string[]` | `[]` | Additional workspace root directories |
| `ignoreDir` | `string[]` | `[]` | Directories to ignore |
| `ignoreGlobs` | `string[]` | `[]` | Glob patterns to ignore |
| `moduleMap` | `array` | `[]` | Module path mappings (regex patterns) |
| `encoding` | `string` | `"utf-8"` | File encoding |
| `enableReindex` | `boolean` | `false` | Enable full reindex on file change |
| `reindexDuration` | `integer` | `5000` | Delay before reindex (ms) |
| `preloadFileSize` | `integer` | `0` | Max file size to preload in bytes (0 = unlimited) |
| `packageDirs` | `string[]` | `[]` | Package directories (partial library load) |

### Library Format

```json
{
  "workspace": {
    "library": [
      "/usr/share/lua/5.1",
      {
        "path": "./mylib",
        "ignoreDir": ["test"],
        "ignoreGlobs": ["**/*.spec.lua"]
      }
    ]
  }
}
```

### Module Map Format

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

---

## Complete Example

```json
{
  "$schema": "https://raw.githubusercontent.com/Pollux-Dev/gmod-glua-ls/main/crates/emmylua_code_analysis/resources/schema.json",
  "gmod": {
    "enabled": true,
    "defaultRealm": "shared",
    "detectRealmFromFilename": true,
    "detectRealmFromCalls": true,
    "inferDynamicFields": true,
    "scriptedClassScopes": {
      "include": [
        "entities/**",
        "weapons/**",
        "effects/**",
        "weapons/gmod_tool/stools/**",
        "plugins/**"
      ],
      "exclude": ["**/tests/**"]
    },
    "hookMappings": {
      "methodToHook": {
        "MyEmitter": "CustomHook"
      },
      "methodPrefixes": ["MYFRAMEWORK"],
      "emitterToHook": {}
    }
  },
  "diagnostics": {
    "enable": true,
    "disable": [],
    "globals": ["MyGlobal"],
    "severity": {
      "unused": "hint",
      "undefined-field": "warning"
    }
  },
  "completion": {
    "enable": true,
    "autoRequire": true,
    "callSnippet": true
  },
  "hint": {
    "enable": true,
    "paramHint": true,
    "localHint": true,
    "indexHint": true,
    "overrideHint": true,
    "metaCallHint": true,
    "enumParamHint": false
  },
  "workspace": {
    "library": ["./glua-api"],
    "ignoreDir": ["build", "dist", "node_modules"],
    "ignoreGlobs": ["*.log", "*.tmp"],
    "encoding": "utf-8"
  },
  "runtime": {
    "extensions": [".lua", ".lua.txt"],
    "nonstandardSymbol": ["continue", "//"]
  },
  "strict": {
    "requirePath": false,
    "arrayIndex": true,
    "metaOverrideFileDefine": true
  },
  "semanticTokens": {
    "enable": true
  },
  "hover": {
    "enable": true
  },
  "references": {
    "enable": true,
    "fuzzySearch": true
  }
}
```
