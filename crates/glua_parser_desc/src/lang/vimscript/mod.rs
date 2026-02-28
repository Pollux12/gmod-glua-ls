mod vim_lexer;

use glua_parser::{LexerState, Reader};
use vim_lexer::{VimTokenKind, VimscriptLexer};

use crate::{CodeBlockHighlightKind, DescItemKind, util::ResultContainer};

pub fn process_vimscript_code_block<'a, C: ResultContainer>(
    c: &mut C,
    line_reader: Reader<'a>,
    state: LexerState,
) -> LexerState {
    let mut lexer = VimscriptLexer::new_with_state(line_reader, state);
    let tokens = lexer.tokenize();
    for token in tokens {
        if !matches!(
            token.kind,
            VimTokenKind::TkEof | VimTokenKind::TkEndOfLine | VimTokenKind::TkWhitespace
        ) {
            let highlight_kind = to_highlight_kind(&token);
            if highlight_kind != CodeBlockHighlightKind::None {
                c.emit_range(token.range, DescItemKind::CodeBlockHl(highlight_kind));
            }
        }
    }

    lexer.get_state()
}

fn to_highlight_kind(token: &vim_lexer::VimTokenData) -> CodeBlockHighlightKind {
    match token.kind {
        VimTokenKind::TkString => CodeBlockHighlightKind::String,
        VimTokenKind::TkNumber => CodeBlockHighlightKind::Number,
        VimTokenKind::TkKeyword => CodeBlockHighlightKind::Keyword,
        VimTokenKind::TkFunction => CodeBlockHighlightKind::Function,
        VimTokenKind::TkVariable => CodeBlockHighlightKind::Variable,
        VimTokenKind::TkOperator => CodeBlockHighlightKind::Operators,
        VimTokenKind::TkComment => CodeBlockHighlightKind::Comment,
        _ => CodeBlockHighlightKind::None,
    }
}
