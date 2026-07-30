<div align="center">

# 📚 GLua Doc CLI

[![Crates.io](https://img.shields.io/crates/v/glua_doc_cli.svg?style=for-the-badge&logo=rust)](https://crates.io/crates/glua_doc_cli)
[![GitHub license](https://img.shields.io/github/license/Pollux12/gmod-glua-ls?style=for-the-badge&logo=mit&color=blue)](../../LICENSE)

</div>

`glua_doc_cli` is a powerful command-line tool for generating documentation directly from your Lua source code and GLua annotations. Built with Rust, it offers exceptional performance and is a core component of the `gmod-glua-ls` ecosystem.

---

## ✨ Features

- **🚀 Blazing Fast**: Leverages Rust's performance to parse and generate documentation for large codebases in seconds.
- **✍️ Rich Annotation Support**: Intelligently interprets GLua annotations (`---@class`, `---@field`, `---@param`, etc.) to generate detailed and accurate documentation.
- **🔧 Highly Customizable**:
    - Override the default templates with `--override-template` to match your project's branding.
    - Inject custom content into the main page using the `--mixin` option to add guides, tutorials, or other static pages.
- **📦 Multiple Output Formats**: Generate MkDocs-compatible **Markdown** or structured **JSON** for maximum flexibility.
- **🤝 CI/CD Ready**: Automate your documentation publishing workflow with seamless integration into services like GitHub Actions.

---

## 📦 Installation

Install `glua_doc_cli` via cargo:
```shell
cargo install glua_doc_cli
```
Alternatively, you can grab pre-built binaries from the [**GitHub Releases**](https://github.com/Pollux12/gmod-glua-ls/releases) page.

---

## 🚀 Usage

### Basic Usage

Generate documentation for all Lua files in the `src` directory. The default output directory is `./output`:
```shell
glua_doc_cli ./src
```

### Advanced Usage

#### Generate JSON Output

Output the documentation structure as a JSON file for custom processing:
```shell
glua_doc_cli . -f json -o ./api.json
```

Use `stdout` as the output destination to pipe JSON to another program:
```shell
glua_doc_cli . --output-format json --output stdout
```

#### Build an HTML Site

The Markdown format writes documentation pages and an `mkdocs.yml` project. It does not compile HTML itself. Install [Material for MkDocs](https://squidfunk.github.io/mkdocs-material/) and build the generated project:
```shell
pip install mkdocs-material
glua_doc_cli . --output-format markdown --output ./output
mkdocs build --config-file ./output/mkdocs.yml
```

#### Customize Site Name

Set a custom name for the generated documentation site:
```shell
glua_doc_cli . -o ./docs --site-name "My Awesome Project"
```

#### Exclude Files or Directories

Exclude a workspace-relative file, directory, or glob. A directory path prunes the complete subtree:
```shell
glua_doc_cli . -o ./docs --exclude ".claude,third_party/**,test/**"
```

---

## Configuration and Requirements

- At least one workspace file or directory is required.
- If `.gluarc.json` exists in the first workspace, it is used exclusively.
- Otherwise, configuration is loaded in this order: `.luarc.json`, `.emmyrc.json`, then `.emmyrc.lua`.
- Unsupported configuration value shapes are ignored one field at a time with a warning, while valid settings continue to apply. Unreadable or malformed configuration files still stop generation. Use `--no-config` to skip configuration discovery entirely.
- Annotation folders should be configured as `workspace.library` entries. Passing an annotation folder as another positional workspace marks it as project code and includes it in the export.

Example annotation library configuration:
```json
{
  "workspace": {
    "library": [
      {
        "path": "C:/path/to/annotations-gmod-glua-ls/output"
      }
    ]
  }
}
```

---

## 🛠️ CI/CD Integration

Automate the process of building and deploying your documentation to GitHub Pages using GitHub Actions.

**Example `.github/workflows/docs.yml`:**
```yaml
name: Generate and Deploy Docs

on:
  push:
    branches:
      - main

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable

      - name: Install glua_doc_cli
        run: cargo install glua_doc_cli

      - name: Generate Docs
        run: glua_doc_cli ./src -o ./docs --site-name "My Project"
```

---

## Command Line Options

```
Usage: glua_doc_cli [OPTIONS] [WORKSPACE]...

Arguments:
  [WORKSPACE]...  Path to the workspace directory

Options:
  -c, --config <CONFIG>                        Configuration file paths. If not provided, ".gluarc.json" takes priority; otherwise ".luarc.json" and legacy Emmy config files are searched in the workspace directory
      --no-config                              Do not discover or load a workspace configuration file
      --include <INCLUDE>                      Comma separated list of include patterns. Patterns must follow glob syntax. It will override the default include patterns.
      --exclude <EXCLUDE>                      Comma separated list of workspace-relative paths or glob patterns. Exclude patterns take precedence over include patterns
  -f, --output-format <OUTPUT_FORMAT>          Specify output format [default: markdown] [possible values: json, markdown]
  -o, --output <OUTPUT>                        Specify output destination (can be stdout when output_format is json) [default: ./output]
      --override-template <OVERRIDE_TEMPLATE>  The path of the override template
      --site-name <SITE_NAME>                  [default: Docs]
      --mixin <MIXIN>                          The path of the mixin md file
      --verbose                                Verbose output
  -h, --help                                   Print help
  -V, --version                                Print version
```

*Footnote: Forked from [EmmyLua Analyzer Rust](https://github.com/CppCXY/emmylua-analyzer-rust).*
