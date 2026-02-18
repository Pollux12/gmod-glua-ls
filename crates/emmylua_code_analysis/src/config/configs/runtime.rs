use std::collections::HashMap;

use emmylua_parser::{LuaNonStdSymbol, LuaVersionNumber, SpecialFunction};
use schemars::JsonSchema;
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EmmyrcRuntime {
    /// Deprecated: GMod-only server always uses LuaJIT. Kept for config compatibility.
    #[serde(default)]
    pub version: EmmyrcLuaVersion,
    #[serde(default)]
    /// Functions that like require.
    pub require_like_function: Vec<String>,
    #[serde(default)]
    /// file Extensions. eg: .lua, .lua.txt
    pub extensions: Vec<String>,
    #[serde(default)]
    /// Require pattern. eg. "?.lua", "?/init.lua"
    pub require_pattern: Vec<String>,
    /// Non-standard symbols.
    #[serde(default)]
    pub nonstandard_symbol: Vec<EmmyrcNonStdSymbol>,
    /// Special symbols.
    #[serde(default)]
    pub special: HashMap<String, EmmyrcSpecialSymbol>,
}

impl Default for EmmyrcRuntime {
    fn default() -> Self {
        Self {
            version: EmmyrcLuaVersion::default(),
            require_like_function: Vec::new(),
            extensions: Vec::new(),
            require_pattern: Vec::new(),
            nonstandard_symbol: default_nonstandard_symbols(),
            special: HashMap::new(),
        }
    }
}

fn default_nonstandard_symbols() -> Vec<EmmyrcNonStdSymbol> {
    vec![
        EmmyrcNonStdSymbol::DoubleSlash,
        EmmyrcNonStdSymbol::SlashStar,
        EmmyrcNonStdSymbol::Continue,
        EmmyrcNonStdSymbol::NotEqual,
        EmmyrcNonStdSymbol::DoublePipe,
        EmmyrcNonStdSymbol::DoubleAmp,
        EmmyrcNonStdSymbol::Exclamation,
    ]
}

// Deprecated: GMod-only server always uses LuaJIT. Kept for config compatibility.
// All variants are retained so old .emmyrc.json files deserialize without errors.
#[derive(Serialize, Deserialize, Debug, JsonSchema, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmmyrcLuaVersion {
    /// Lua 5.1
    #[serde(rename = "Lua5.1", alias = "Lua 5.1")]
    Lua51,
    /// LuaJIT
    #[serde(rename = "LuaJIT")]
    #[default]
    LuaJIT,
    /// Lua 5.2
    #[serde(rename = "Lua5.2", alias = "Lua 5.2")]
    Lua52,
    /// Lua 5.3
    #[serde(rename = "Lua5.3", alias = "Lua 5.3")]
    Lua53,
    /// Lua 5.4
    #[serde(rename = "Lua5.4", alias = "Lua 5.4")]
    Lua54,
    /// Lua 5.5
    #[serde(rename = "Lua5.5", alias = "Lua 5.5")]
    Lua55,
    /// Lua Latest
    #[serde(rename = "LuaLatest", alias = "Lua Latest")]
    LuaLatest,
}

impl EmmyrcLuaVersion {
    pub fn to_lua_version_number(&self) -> LuaVersionNumber {
        match self {
            EmmyrcLuaVersion::Lua51 => LuaVersionNumber::new(5, 1, 0),
            EmmyrcLuaVersion::LuaJIT => LuaVersionNumber::LUA_JIT,
            EmmyrcLuaVersion::Lua52 => LuaVersionNumber::new(5, 2, 0),
            EmmyrcLuaVersion::Lua53 => LuaVersionNumber::new(5, 3, 0),
            EmmyrcLuaVersion::Lua54 => LuaVersionNumber::new(5, 4, 0),
            EmmyrcLuaVersion::LuaLatest => LuaVersionNumber::new(5, 4, 0),
            EmmyrcLuaVersion::Lua55 => LuaVersionNumber::new(5, 5, 0),
        }
    }
}

#[allow(unused)]
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum EmmyrcNonStdSymbol {
    #[serde(rename = "//")]
    DoubleSlash = 1, // "//"
    #[serde(rename = "/**/")]
    SlashStar, // "/**/"
    #[serde(rename = "`")]
    Backtick, // "`"
    #[serde(rename = "+=")]
    PlusAssign, // "+="
    #[serde(rename = "-=")]
    MinusAssign, // "-="
    #[serde(rename = "*=")]
    StarAssign, // "*="
    #[serde(rename = "/=")]
    SlashAssign, // "/="
    #[serde(rename = "%=")]
    PercentAssign, // "%="
    #[serde(rename = "^=")]
    CaretAssign, // "^="
    #[serde(rename = "//=")]
    DoubleSlashAssign, // "//="
    #[serde(rename = "|=")]
    PipeAssign, // "|="
    #[serde(rename = "&=")]
    AmpAssign, // "&="
    #[serde(rename = "<<=")]
    ShiftLeftAssign, // "<<="
    #[serde(rename = ">>=")]
    ShiftRightAssign, // ">>="
    #[serde(rename = "||")]
    DoublePipe, // "||"
    #[serde(rename = "&&")]
    DoubleAmp, // "&&"
    #[serde(rename = "!")]
    Exclamation, // "!"
    #[serde(rename = "!=")]
    NotEqual, // "!="
    #[serde(rename = "continue")]
    Continue, // "continue"
}

impl From<EmmyrcNonStdSymbol> for LuaNonStdSymbol {
    fn from(symbol: EmmyrcNonStdSymbol) -> Self {
        match symbol {
            EmmyrcNonStdSymbol::DoubleSlash => LuaNonStdSymbol::DoubleSlash,
            EmmyrcNonStdSymbol::SlashStar => LuaNonStdSymbol::SlashStar,
            EmmyrcNonStdSymbol::Backtick => LuaNonStdSymbol::Backtick,
            EmmyrcNonStdSymbol::PlusAssign => LuaNonStdSymbol::PlusAssign,
            EmmyrcNonStdSymbol::MinusAssign => LuaNonStdSymbol::MinusAssign,
            EmmyrcNonStdSymbol::StarAssign => LuaNonStdSymbol::StarAssign,
            EmmyrcNonStdSymbol::SlashAssign => LuaNonStdSymbol::SlashAssign,
            EmmyrcNonStdSymbol::PercentAssign => LuaNonStdSymbol::PercentAssign,
            EmmyrcNonStdSymbol::CaretAssign => LuaNonStdSymbol::CaretAssign,
            EmmyrcNonStdSymbol::DoubleSlashAssign => LuaNonStdSymbol::DoubleSlashAssign,
            EmmyrcNonStdSymbol::PipeAssign => LuaNonStdSymbol::PipeAssign,
            EmmyrcNonStdSymbol::AmpAssign => LuaNonStdSymbol::AmpAssign,
            EmmyrcNonStdSymbol::ShiftLeftAssign => LuaNonStdSymbol::ShiftLeftAssign,
            EmmyrcNonStdSymbol::ShiftRightAssign => LuaNonStdSymbol::ShiftRightAssign,
            EmmyrcNonStdSymbol::DoublePipe => LuaNonStdSymbol::DoublePipe,
            EmmyrcNonStdSymbol::DoubleAmp => LuaNonStdSymbol::DoubleAmp,
            EmmyrcNonStdSymbol::Exclamation => LuaNonStdSymbol::Exclamation,
            EmmyrcNonStdSymbol::NotEqual => LuaNonStdSymbol::NotEqual,
            EmmyrcNonStdSymbol::Continue => LuaNonStdSymbol::Continue,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmmyrcSpecialSymbol {
    #[serde(rename = "none")]
    None,
    Require,
    Error,
    Assert,
    Type,
    Setmetatable,
}

impl<'de> Deserialize<'de> for EmmyrcSpecialSymbol {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // 首先尝试使用默认的 derive 实现
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "none" => Ok(EmmyrcSpecialSymbol::None),
            "require" => Ok(EmmyrcSpecialSymbol::Require),
            "error" => Ok(EmmyrcSpecialSymbol::Error),
            "assert" => Ok(EmmyrcSpecialSymbol::Assert),
            "type" => Ok(EmmyrcSpecialSymbol::Type),
            "setmetatable" => Ok(EmmyrcSpecialSymbol::Setmetatable),
            // 对于任何不匹配的值，返回 None
            _ => Ok(EmmyrcSpecialSymbol::None),
        }
    }
}

impl From<EmmyrcSpecialSymbol> for Option<SpecialFunction> {
    fn from(symbol: EmmyrcSpecialSymbol) -> Self {
        match symbol {
            EmmyrcSpecialSymbol::None => None,
            EmmyrcSpecialSymbol::Require => Some(SpecialFunction::Require),
            EmmyrcSpecialSymbol::Error => Some(SpecialFunction::Error),
            EmmyrcSpecialSymbol::Assert => Some(SpecialFunction::Assert),
            EmmyrcSpecialSymbol::Type => Some(SpecialFunction::Type),
            EmmyrcSpecialSymbol::Setmetatable => Some(SpecialFunction::Setmetaatable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_emmyrc_runtime() {
        let json1 = r#"{
            "version": "Lua5.1"
        }"#;
        let runtime: EmmyrcRuntime = serde_json::from_str(json1).unwrap();
        assert_eq!(runtime.version, EmmyrcLuaVersion::Lua51);

        let json2 = r#"{
            "version": "Lua 5.1"
        }"#;

        let runtime: EmmyrcRuntime = serde_json::from_str(json2).unwrap();
        assert_eq!(runtime.version, EmmyrcLuaVersion::Lua51);
    }

    #[test]
    fn test_runtime_default_has_glua_nonstandard_symbols() {
        let runtime = EmmyrcRuntime::default();
        assert!(
            runtime
                .nonstandard_symbol
                .contains(&EmmyrcNonStdSymbol::DoubleSlash)
        );
        assert!(
            runtime
                .nonstandard_symbol
                .contains(&EmmyrcNonStdSymbol::SlashStar)
        );
        assert!(
            runtime
                .nonstandard_symbol
                .contains(&EmmyrcNonStdSymbol::Continue)
        );
        assert!(
            runtime
                .nonstandard_symbol
                .contains(&EmmyrcNonStdSymbol::NotEqual)
        );
        assert!(
            runtime
                .nonstandard_symbol
                .contains(&EmmyrcNonStdSymbol::DoublePipe)
        );
        assert!(
            runtime
                .nonstandard_symbol
                .contains(&EmmyrcNonStdSymbol::DoubleAmp)
        );
        assert!(
            runtime
                .nonstandard_symbol
                .contains(&EmmyrcNonStdSymbol::Exclamation)
        );
    }
}
