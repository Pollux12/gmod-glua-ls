mod sql_lexer;

use glua_parser::{LexerState, Reader};

use crate::{
    CodeBlockHighlightKind, DescItemKind,
    lang::sql::sql_lexer::{SqlLexer, SqlTokenData, SqlTokenKind},
    util::ResultContainer,
};

pub fn process_sql_code_block<'a, C: ResultContainer>(
    c: &mut C,
    line_reader: Reader<'a>,
    state: LexerState,
) -> LexerState {
    let mut lexer = SqlLexer::new_with_state(line_reader, state);

    let tokens = lexer.tokenize();
    for token in tokens {
        if !matches!(token.kind, SqlTokenKind::TkEof | SqlTokenKind::TkWhitespace) {
            let highlight_kind = to_highlight_kind(&token);
            if highlight_kind != CodeBlockHighlightKind::None {
                c.emit_range(token.range, DescItemKind::CodeBlockHl(highlight_kind));
            }
        }
    }

    lexer.get_state()
}

fn to_highlight_kind(token: &SqlTokenData) -> CodeBlockHighlightKind {
    match token.kind {
        SqlTokenKind::TkKeyword => CodeBlockHighlightKind::Keyword,
        SqlTokenKind::TkDataType => CodeBlockHighlightKind::Class,
        SqlTokenKind::TkFunction => CodeBlockHighlightKind::Function,
        SqlTokenKind::TkSingleQuotedString
        | SqlTokenKind::TkDoubleQuotedString
        | SqlTokenKind::TkBacktickString => CodeBlockHighlightKind::String,
        SqlTokenKind::TkInteger | SqlTokenKind::TkFloat | SqlTokenKind::TkHexNumber => {
            CodeBlockHighlightKind::Number
        }
        SqlTokenKind::TkLineComment | SqlTokenKind::TkBlockComment => {
            CodeBlockHighlightKind::Comment
        }
        SqlTokenKind::TkOperator => CodeBlockHighlightKind::Operators,
        SqlTokenKind::TkParameter => CodeBlockHighlightKind::Variable,
        SqlTokenKind::TkIdentifier => CodeBlockHighlightKind::Variable,
        SqlTokenKind::TkSemicolon
        | SqlTokenKind::TkComma
        | SqlTokenKind::TkDot
        | SqlTokenKind::TkLeftParen
        | SqlTokenKind::TkRightParen
        | SqlTokenKind::TkLeftBracket
        | SqlTokenKind::TkRightBracket => CodeBlockHighlightKind::Operators,
        _ => CodeBlockHighlightKind::None,
    }
}
