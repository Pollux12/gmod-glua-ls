mod json_lexer;

use glua_parser::{LexerState, Reader};

use crate::{
    CodeBlockHighlightKind, DescItemKind,
    lang::json::json_lexer::{JsonLexer, JsonTokenData, JsonTokenKind},
    util::ResultContainer,
};

pub fn process_json_code_block<'a, C: ResultContainer>(
    c: &mut C,
    line_reader: Reader<'a>,
    state: LexerState,
) -> LexerState {
    let mut lexer = JsonLexer::new_with_state(line_reader, state);
    let tokens = lexer.tokenize();
    for token in tokens {
        if !matches!(
            token.kind,
            JsonTokenKind::TkEof | JsonTokenKind::TkWhitespace
        ) {
            let highlight_kind = to_highlight_kind(&token);
            if highlight_kind != CodeBlockHighlightKind::None {
                c.emit_range(token.range, DescItemKind::CodeBlockHl(highlight_kind));
            }
        }
    }

    lexer.get_state()
}

fn to_highlight_kind(token: &JsonTokenData) -> CodeBlockHighlightKind {
    match token.kind {
        JsonTokenKind::TkString => CodeBlockHighlightKind::String,
        JsonTokenKind::TkNumber => CodeBlockHighlightKind::Number,
        JsonTokenKind::TkKeyword => CodeBlockHighlightKind::Keyword,
        JsonTokenKind::TkLeftBrace
        | JsonTokenKind::TkRightBrace
        | JsonTokenKind::TkLeftBracket
        | JsonTokenKind::TkRightBracket
        | JsonTokenKind::TkColon
        | JsonTokenKind::TkComma => CodeBlockHighlightKind::Operators,
        _ => CodeBlockHighlightKind::None,
    }
}
