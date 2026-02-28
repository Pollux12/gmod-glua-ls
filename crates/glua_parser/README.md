## EmmyLua-Parser

EmmyLua-Parser is a parser for Lua5.1, Lua5.2, Lua5.3, Lua5.4, and LuaJIT and also supports EmmyLua/LuaCats annotations. Its purpose is to generate AST and CST from the parsed code for further analysis.

### Internationalization (i18n) Support

This crate supports multiple languages, defaulting to English (en-US). Users can optionally initialize i18n to set a different language.

### Features

- Lossless syntax tree generation
- Easy-to-use API based on the `rowan` library
- Support for Lua5.1, Lua5.2, Lua5.3, Lua5.4, Lua5.5 and LuaJIT
- Support for EmmyLua/LuaCats annotations
- Ability to parse code with syntax errors

### Usage

```rust
let code = r#"
    local a = 1
    local b = 2
    print(a + b)
"#;
let tree = LuaParser::parse(code, ParserConfig::default());

let chunk = tree.get_chunk_node();
for node in chunk.descendants::<LuaAst>() {
    println!("{:?}", node);
}
```
