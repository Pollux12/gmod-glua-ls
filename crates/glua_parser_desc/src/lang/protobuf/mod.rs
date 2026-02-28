mod protobuf_lexer;

use glua_parser::{LexerState, Reader};

use crate::{
    CodeBlockHighlightKind, DescItemKind,
    lang::protobuf::protobuf_lexer::{ProtobufLexer, ProtobufTokenData, ProtobufTokenKind},
    util::ResultContainer,
};

pub fn process_protobuf_code_block<'a, C: ResultContainer>(
    c: &mut C,
    line_reader: Reader<'a>,
    state: LexerState,
) -> LexerState {
    let mut lexer = ProtobufLexer::new_with_state(line_reader, state);
    let tokens = lexer.tokenize();
    for token in tokens {
        if !matches!(
            token.kind,
            ProtobufTokenKind::TkEof | ProtobufTokenKind::TkWhitespace
        ) {
            let highlight_kind = to_highlight_kind(&token);
            if highlight_kind != CodeBlockHighlightKind::None {
                c.emit_range(token.range, DescItemKind::CodeBlockHl(highlight_kind));
            }
        }
    }

    lexer.get_state()
}

fn to_highlight_kind(token: &ProtobufTokenData) -> CodeBlockHighlightKind {
    match token.kind {
        ProtobufTokenKind::TkKeyword => CodeBlockHighlightKind::Keyword,
        ProtobufTokenKind::TkType => CodeBlockHighlightKind::Class,
        ProtobufTokenKind::TkString => CodeBlockHighlightKind::String,
        ProtobufTokenKind::TkNumber | ProtobufTokenKind::TkFloat => CodeBlockHighlightKind::Number,
        ProtobufTokenKind::TkLineComment | ProtobufTokenKind::TkBlockComment => {
            CodeBlockHighlightKind::Comment
        }
        ProtobufTokenKind::TkIdentifier => CodeBlockHighlightKind::Variable,
        ProtobufTokenKind::TkSemicolon
        | ProtobufTokenKind::TkComma
        | ProtobufTokenKind::TkDot
        | ProtobufTokenKind::TkEquals
        | ProtobufTokenKind::TkLeftParen
        | ProtobufTokenKind::TkRightParen
        | ProtobufTokenKind::TkLeftBrace
        | ProtobufTokenKind::TkRightBrace
        | ProtobufTokenKind::TkLeftBracket
        | ProtobufTokenKind::TkRightBracket
        | ProtobufTokenKind::TkLeftAngle
        | ProtobufTokenKind::TkRightAngle
        | ProtobufTokenKind::TkColon
        | ProtobufTokenKind::TkSlash
        | ProtobufTokenKind::TkMinus
        | ProtobufTokenKind::TkPlus => CodeBlockHighlightKind::Operators,
        _ => CodeBlockHighlightKind::None,
    }
}
