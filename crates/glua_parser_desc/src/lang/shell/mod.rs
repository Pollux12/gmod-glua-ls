mod shell_lexer;

use glua_parser::{LexerState, Reader};

use crate::{CodeBlockHighlightKind, DescItemKind, util::ResultContainer};

use shell_lexer::{ShellLexer, ShellTokenData, ShellTokenKind};

pub fn process_shell_code_block<'a, C: ResultContainer>(
    c: &mut C,
    line_reader: Reader<'a>,
    state: LexerState,
) -> LexerState {
    let mut lexer = ShellLexer::new_with_state(line_reader, state);
    let tokens = lexer.tokenize();
    for token in tokens {
        if !matches!(
            token.kind,
            ShellTokenKind::TkEof | ShellTokenKind::TkWhitespace
        ) {
            let highlight_kind = to_highlight_kind(&token);
            if highlight_kind != CodeBlockHighlightKind::None {
                c.emit_range(token.range, DescItemKind::CodeBlockHl(highlight_kind));
            }
        }
    }

    lexer.get_state()
}

fn to_highlight_kind(token: &ShellTokenData) -> CodeBlockHighlightKind {
    match token.kind {
        ShellTokenKind::TkString
        | ShellTokenKind::TkSingleQuotedString
        | ShellTokenKind::TkDoubleQuotedString
        | ShellTokenKind::TkBacktickString
        | ShellTokenKind::TkHereDoc => CodeBlockHighlightKind::String,

        ShellTokenKind::TkNumber => CodeBlockHighlightKind::Number,

        ShellTokenKind::TkKeyword => CodeBlockHighlightKind::Keyword,

        ShellTokenKind::TkBuiltin | ShellTokenKind::TkCommand => CodeBlockHighlightKind::Function,

        ShellTokenKind::TkVariable | ShellTokenKind::TkDollar => CodeBlockHighlightKind::Variable,

        ShellTokenKind::TkComment => CodeBlockHighlightKind::Comment,

        ShellTokenKind::TkOperator
        | ShellTokenKind::TkPipe
        | ShellTokenKind::TkRedirection
        | ShellTokenKind::TkBackground
        | ShellTokenKind::TkAnd
        | ShellTokenKind::TkOr => CodeBlockHighlightKind::Operators,

        ShellTokenKind::TkLeftBrace
        | ShellTokenKind::TkRightBrace
        | ShellTokenKind::TkLeftBracket
        | ShellTokenKind::TkRightBracket
        | ShellTokenKind::TkLeftParen
        | ShellTokenKind::TkRightParen
        | ShellTokenKind::TkSemicolon => CodeBlockHighlightKind::Operators,

        _ => CodeBlockHighlightKind::None,
    }
}
