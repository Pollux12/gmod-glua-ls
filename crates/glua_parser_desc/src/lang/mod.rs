mod json;
mod lua;
mod protobuf;
mod shell;
mod sql;
mod vimscript;

use glua_parser::{LexerState, Reader, SourceRange};

use crate::{
    DescItemKind,
    lang::{
        json::process_json_code_block, lua::process_lua_code_block,
        protobuf::process_protobuf_code_block, shell::process_shell_code_block,
        sql::process_sql_code_block, vimscript::process_vimscript_code_block,
    },
    util::ResultContainer,
};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CodeBlockLang {
    None,
    Lua,
    Vimscript,
    Json,
    Shell,
    Sql,
    Protobuf,
    Other,
}

impl CodeBlockLang {
    pub fn try_parse(lang: &str) -> Option<Self> {
        match lang {
            "lua" | "Lua" => Some(CodeBlockLang::Lua),
            "" | "none" => Some(CodeBlockLang::None),
            "vim" | "vimscript" => Some(CodeBlockLang::Vimscript),
            "json" | "Json" => Some(CodeBlockLang::Json),
            "shell" | "Shell" => Some(CodeBlockLang::Shell),
            "sql" | "Sql" => Some(CodeBlockLang::Sql),
            "Protobuf" | "protobuf" => Some(CodeBlockLang::Protobuf),
            _ => Some(CodeBlockLang::Other),
        }
    }
}

pub fn process_code<'a, C: ResultContainer>(
    c: &mut C,
    range: SourceRange,
    reader: Reader<'a>,
    state: LexerState,
    lang: CodeBlockLang,
) -> LexerState {
    match lang {
        CodeBlockLang::Lua => process_lua_code_block(c, reader, state),
        CodeBlockLang::Vimscript => process_vimscript_code_block(c, reader, state),
        CodeBlockLang::Json => process_json_code_block(c, reader, state),
        CodeBlockLang::Shell => process_shell_code_block(c, reader, state),
        CodeBlockLang::Sql => process_sql_code_block(c, reader, state),
        CodeBlockLang::Protobuf => process_protobuf_code_block(c, reader, state),
        _ => {
            c.emit_range(range, DescItemKind::CodeBlock);
            state
        }
    }
}
