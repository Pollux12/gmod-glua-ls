<div align="center">

# 🦀 GLua Check

[![Crates.io](https://img.shields.io/crates/v/glua_check.svg?style=for-the-badge&logo=rust)](https://crates.io/crates/glua_check)
[![GitHub license](https://img.shields.io/github/license/Pollux12/gmod-glua-ls?style=for-the-badge&logo=mit&color=blue)](../../LICENSE)

</div>

`glua_check` is a powerful command-line tool designed to help developers identify and fix potential issues in Lua code during development. It leverages the core analysis engine of `gmod-glua-ls` to provide comprehensive code diagnostics, ensuring code quality and robustness.

---

## ✨ Features

- **⚡ High Performance**: Built with Rust for blazing-fast analysis, capable of handling large codebases.
- **🎯 Comprehensive Diagnostics**: Offers over 50 types of diagnostics, including:
  - Syntax errors
  - Type mismatches
  - Undefined variables and fields
  - Unused code
  - Code style issues
  - ...and more!
- **⚙️ Highly Configurable**: Fine-grained control over each diagnostic via `.gluarc.json` or `.luarc.json` files, including enabling/disabling and severity levels.
- **💻 Cross-Platform**: Supports Windows, macOS, and Linux.
- **CI/CD Friendly**: Easily integrates into continuous integration workflows to ensure team code quality.

---

## 📦 Installation

Install `glua_check` via cargo:
```shell
cargo install glua_check
```

---

## 🚀 Usage

### Basic Usage

Analyze all Lua files in the current directory:
```shell
glua_check .
```

Analyze a specific workspace directory:
```shell
glua_check ./src
```

### Advanced Usage

#### Specify Configuration File

Use a specific `.gluarc.json` configuration file:
```shell
glua_check . -c ./config/.gluarc.json
```

#### Ignore Specific Files or Directories

Ignore files in the `vender` and `test` directories:
```shell
glua_check . -i "vender/**,test/**"
```

#### Output in JSON Format

Output diagnostics in JSON format to a file for further processing:
```shell
glua_check . -f json --output ./diag.json
```

---

## ⚙️ Configuration

`glua_check` shares the same configuration system as the GLua Language Server. You can create a `.gluarc.json` file in your project root to configure diagnostic rules.

**Example `.gluarc.json`:**
```json
{
  "diagnostics": {
    "disable": [
      "unused"
    ]
  }
}
```

For detailed information on all available diagnostics and configuration options, see the [**GLua Configuration Documentation**](../../docs/config.md).

---

## 🛠️ CI/CD Integration

You can easily integrate `glua_check` into your GitHub Actions workflow to automate code checks.

**Example `.github/workflows/check.yml`:**
```yaml
name: GLua Check

on: [push, pull_request]

jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
      - name: Install glua_check
        run: cargo install glua_check
      - name: Run check
        run: glua_check .
```

---

## Command Line Options

```
Usage: glua_check [OPTIONS] [WORKSPACE]...

Arguments:
  [WORKSPACE]...  Path(s) to workspace directory

Options:
  -c, --config <CONFIG>                Path to configuration file. If not provided, ".gluarc.json" takes priority; otherwise ".luarc.json" and legacy Emmy and LuaLS config files are searched in the workspace directory
  -i, --ignore <IGNORE>                Comma-separated list of ignore patterns. Patterns must follow glob syntax
  -f, --output-format <OUTPUT_FORMAT>  Specify output format [default: text] [possible values: json, text]
      --output <OUTPUT>                Specify output target (stdout or file path, only used when output_format is json) [default: stdout]
      --warnings-as-errors             Treat warnings as errors
      --verbose                        Verbose output
  -h, --help                           Print help information
  -V, --version                        Print version information
```

*Based on [EmmyLua Analyzer Rust](https://github.com/CppCXY/emmylua-analyzer-rust).*
