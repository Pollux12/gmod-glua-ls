# schema_to_glua

A tool and library that converts JSON Schema into EmmyLua annotations.

Concise and suitable for automatically generating Lua/EmmyLua comments from backend or normalized JSON Schema (for IDE type hints and static checking).

## Features
- Supports basic JSON Schema types: object, array, string, number, integer, boolean, null
- Generates @class, @field, @alias and other EmmyLua annotations
- Provides CLI and library interfaces, can be embedded into build pipelines

## Install / Build
Build via Rust (requires Rust toolchain):
```bash
cargo install schema_to_glua
```

## CLI Usage
Convert schema.json to stdout:
```bash
schema_to_glua schema_to_glua <schema.json> [output.lua]
```

## Library Usage (Example)
Add dependency (Cargo.toml):
```toml
schema_to_glua = "0.1.0"
```
Example code:
```rust
let schema = r#"
{
    "type": "object",
    "properties": {
        "name": { "type": "string" },
        "age": { "type": "integer" }
    },
    "required": ["name"]
}
"#;

let converter = SchemaConverter::new(true);
let emmylua = converter.convert_from_str(&schema);
```

## Input Example (schema.json)
```json
{
    "type": "object",
    "properties": {
        "name": { "type": "string" },
        "age": { "type": "integer" },
        "tags": {
            "type": "array",
            "items": { "type": "string" }
        }
    },
    "required": ["name"]
}
```

## Generated EmmyLua Example
```lua
---@class Person
---@field public name string
---@field public age integer|nil
---@field public tags string[]|nil
```

## License
MIT License
