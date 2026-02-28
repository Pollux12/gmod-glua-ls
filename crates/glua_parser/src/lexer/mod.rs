mod lexer_config;
mod lua_doc_lexer;
mod lua_lexer;
mod test;
mod token_data;

pub use lexer_config::LexerConfig;
pub use lua_doc_lexer::{LuaDocLexer, LuaDocLexerState};
pub use lua_lexer::LuaLexer;
pub use token_data::LuaTokenData;

fn is_name_start(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_'
}

fn is_name_continue(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// This enum allows preserving lexer state between reader resets. This is used
/// when lexer doesn't see the whole input source, and only sees a reader
/// for each individual line. It happens when we're lexing
/// code blocks in comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexerState {
    Normal,
    String(char),
    LongString(usize),
    LongComment(usize),
}
