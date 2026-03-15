# Emmyrc JSON Configuration (EN)

## `strict`

| Option | Type | Default | Description |
| -------- | ------ | --------- | ------------- |
| `requirePath` | `boolean` | `false` | Strict require path checking |
| `arrayIndex` | `boolean` | `false` | Strict array index checking |
| `metaOverrideFileDefine` | `boolean` | `true` | Meta definitions override file definitions |
| `docBaseConstMatchBaseType` | `boolean` | `true` | Allow base constants to match base types |
| `requireExportGlobal` | `boolean` | `false` | Require `---@export global` for library visibility |
| `allowNullableAsNonNullable` | `boolean` | `true` | Allow nullable types (`T?`) to be passed where non-nullable (`T`) is expected |
| `inferredTypeMismatch` | `boolean` | `false` | Report mismatch diagnostics for inferred types instead of keeping inferred values lenient |
