use glua_parser::LuaSyntaxToken;
use lsp_types::{SemanticTokenModifier, SemanticTokenType};
use rowan::{TextRange, TextSize};

use crate::handlers::semantic_token::semantic_token_builder::SemanticBuilder;

/// Highlight a regular (non-long) Lua string token, emitting the escape sequences as
/// separate semantic tokens so editors can color them distinctly.
///
/// The string is split into non-overlapping segments: literal runs (including the
/// surrounding quotes) are emitted as `STRING`, valid escape sequences as
/// `STRING + MODIFICATION`, and invalid escape sequences as `STRING + DEPRECATED`.
///
/// Only call this for `TkString` tokens. Long strings (`[[ ... ]]`) do not process
/// escape sequences and must keep their single `STRING` token.
pub fn highlight_string_escapes(builder: &mut SemanticBuilder, token: &LuaSyntaxToken) {
    let text = token.text();
    let base = token.text_range().start();

    // Byte offset (within `text`) where the current literal run started.
    let mut run_start: usize = 0;
    let mut chars = text.char_indices().peekable();

    while let Some((idx, c)) = chars.next() {
        if c != '\\' {
            continue;
        }

        // Flush the literal run before this backslash.
        push_segment(builder, text, base, run_start, idx, SegmentKind::Literal);

        // Consume the escape sequence and determine whether it is valid.
        let valid = consume_escape(&mut chars);
        // Where the escape ends: the next char_indices position, or end of text.
        let escape_end = chars.peek().map(|(i, _)| *i).unwrap_or(text.len());
        let kind = if valid {
            SegmentKind::ValidEscape
        } else {
            SegmentKind::InvalidEscape
        };
        push_segment(builder, text, base, idx, escape_end, kind);

        run_start = escape_end;
    }

    // Flush the trailing literal run (includes the closing quote, if any).
    push_segment(
        builder,
        text,
        base,
        run_start,
        text.len(),
        SegmentKind::Literal,
    );
}

enum SegmentKind {
    Literal,
    ValidEscape,
    InvalidEscape,
}

fn push_segment(
    builder: &mut SemanticBuilder,
    text: &str,
    base: TextSize,
    start: usize,
    end: usize,
    kind: SegmentKind,
) {
    if start >= end {
        return;
    }
    let slice = &text[start..end];
    let range = TextRange::new(
        base + TextSize::from(start as u32),
        base + TextSize::from(end as u32),
    );
    let modifiers: &[SemanticTokenModifier] = match kind {
        SegmentKind::Literal => &[],
        SegmentKind::ValidEscape => &[SemanticTokenModifier::MODIFICATION],
        SegmentKind::InvalidEscape => &[SemanticTokenModifier::DEPRECATED],
    };
    builder.push_at_range(slice, range, SemanticTokenType::STRING, modifiers);
}

/// Consume the characters of an escape sequence after the leading backslash has already
/// been read. The iterator is positioned just past the backslash. Returns whether the
/// escape sequence is valid.
///
/// Mirrors `normal_string_value` in
/// `crates/glua_parser/src/syntax/node/token/string_analyzer.rs`.
fn consume_escape<I>(chars: &mut std::iter::Peekable<I>) -> bool
where
    I: Iterator<Item = (usize, char)>,
{
    let Some((_, next)) = chars.next() else {
        // Trailing backslash at end of token: invalid.
        return false;
    };

    match next {
        'a' | 'b' | 'f' | 'n' | 'r' | 't' | 'v' | '\\' | '\'' | '"' | '\r' | '\n' => true,
        'z' => {
            while let Some((_, c)) = chars.peek() {
                if !c.is_ascii_whitespace() {
                    break;
                }
                chars.next();
            }
            true
        }
        'x' => {
            // Exactly two hex digits.
            let mut count = 0;
            while count < 2 {
                match chars.peek() {
                    Some((_, d)) if d.is_ascii_hexdigit() => {
                        chars.next();
                        count += 1;
                    }
                    _ => break,
                }
            }
            count == 2
        }
        'u' => {
            // \u{ hex+ }
            if !matches!(chars.peek(), Some((_, '{'))) {
                return false;
            }
            chars.next(); // consume '{'
            let mut hex = String::new();
            let mut closed = false;
            while let Some((_, d)) = chars.peek().copied() {
                if d == '}' {
                    chars.next();
                    closed = true;
                    break;
                }
                if !d.is_ascii_hexdigit() {
                    break;
                }
                hex.push(d);
                chars.next();
            }
            if !closed || hex.is_empty() {
                return false;
            }
            // Must be a valid Unicode scalar value.
            u32::from_str_radix(&hex, 16)
                .ok()
                .and_then(char::from_u32)
                .is_some()
        }
        '0'..='9' => {
            // Up to three decimal digits total, value must fit in a byte (0-255).
            let mut dec = String::new();
            dec.push(next);
            while dec.len() < 3 {
                match chars.peek() {
                    Some((_, d)) if d.is_ascii_digit() => {
                        dec.push(*d);
                        chars.next();
                    }
                    _ => break,
                }
            }
            dec.parse::<u8>().is_ok()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk a raw string token's text the same way `highlight_string_escapes` does and
    /// return the segments as (relative_start, len, kind) tuples. This exercises the
    /// segmentation logic without constructing a real `SemanticBuilder`.
    fn segments(text: &str) -> Vec<(usize, usize, &'static str)> {
        let mut out = Vec::new();
        let mut run_start = 0usize;
        let mut chars = text.char_indices().peekable();

        while let Some((idx, c)) = chars.next() {
            if c != '\\' {
                continue;
            }
            if idx > run_start {
                out.push((run_start, idx - run_start, "string"));
            }
            let valid = consume_escape(&mut chars);
            let escape_end = chars.peek().map(|(i, _)| *i).unwrap_or(text.len());
            out.push((
                idx,
                escape_end - idx,
                if valid { "escape" } else { "invalid" },
            ));
            run_start = escape_end;
        }
        if text.len() > run_start {
            out.push((run_start, text.len() - run_start, "string"));
        }
        out
    }

    #[test]
    fn plain_string_is_single_run() {
        assert_eq!(segments("\"abc\""), vec![(0, 5, "string")]);
    }

    #[test]
    fn empty_and_unterminated() {
        assert_eq!(segments("\"\""), vec![(0, 2, "string")]);
        assert_eq!(segments("\""), vec![(0, 1, "string")]);
        assert_eq!(segments("\"abc"), vec![(0, 4, "string")]);
    }

    #[test]
    fn simple_escapes() {
        // "a\n" -> `"a`, `\n`, `"`
        assert_eq!(
            segments("\"a\\n\""),
            vec![(0, 2, "string"), (2, 2, "escape"), (4, 1, "string")]
        );
        // backslash, quote, tab, z, bell
        for esc in ["\\\\", "\\\"", "\\t", "\\z", "\\a"] {
            let s = format!("\"{esc}\"");
            assert_eq!(
                segments(&s),
                vec![(0, 1, "string"), (1, 2, "escape"), (3, 1, "string")],
                "escape {esc}"
            );
        }
    }

    #[test]
    fn line_continuation_escape() {
        // \<newline>
        assert_eq!(
            segments("\"\\\n\""),
            vec![(0, 1, "string"), (1, 2, "escape"), (3, 1, "string")]
        );
    }

    #[test]
    fn z_escape_consumes_following_whitespace() {
        assert_eq!(
            segments("\"a\\z\n  b\""),
            vec![(0, 2, "string"), (2, 5, "escape"), (7, 2, "string")]
        );
    }

    #[test]
    fn z_escape_does_not_consume_unicode_whitespace() {
        assert_eq!(
            segments("\"\\z\u{00a0}x\""),
            vec![(0, 1, "string"), (1, 2, "escape"), (3, 4, "string")]
        );
    }

    #[test]
    fn decimal_escapes() {
        // \65 (three given but only valid up to 255)
        assert_eq!(
            segments("\"\\65\""),
            vec![(0, 1, "string"), (1, 3, "escape"), (4, 1, "string")]
        );
        // \9 single digit
        assert_eq!(
            segments("\"\\9\""),
            vec![(0, 1, "string"), (1, 2, "escape"), (3, 1, "string")]
        );
        // \255 ok, \256 overflows a byte -> invalid
        assert_eq!(
            segments("\"\\255\""),
            vec![(0, 1, "string"), (1, 4, "escape"), (5, 1, "string")]
        );
        assert_eq!(
            segments("\"\\256\""),
            vec![(0, 1, "string"), (1, 4, "invalid"), (5, 1, "string")]
        );
    }

    #[test]
    fn hex_escapes() {
        // \x41 -> `\x41`
        assert_eq!(
            segments("\"\\x41\""),
            vec![(0, 1, "string"), (1, 4, "escape"), (5, 1, "string")]
        );
        // \x4 (only one hex digit) -> invalid, length covers `\x4`
        assert_eq!(
            segments("\"\\x4\""),
            vec![(0, 1, "string"), (1, 3, "invalid"), (4, 1, "string")]
        );
        // \xZZ (no hex digits) -> invalid, length covers `\x`
        assert_eq!(
            segments("\"\\xZZ\""),
            vec![
                (0, 1, "string"),
                (1, 2, "invalid"),
                (3, 3, "string") // ZZ"
            ]
        );
    }

    #[test]
    fn unicode_escapes() {
        // \u{48} valid
        assert_eq!(
            segments("\"\\u{48}\""),
            vec![(0, 1, "string"), (1, 6, "escape"), (7, 1, "string")]
        );
        // \u{} empty -> invalid
        assert_eq!(
            segments("\"\\u{}\""),
            vec![(0, 1, "string"), (1, 4, "invalid"), (5, 1, "string")]
        );
        // \u48 missing brace -> invalid (covers just `\u`)
        assert_eq!(
            segments("\"\\u48\""),
            vec![(0, 1, "string"), (1, 2, "invalid"), (3, 3, "string")]
        );
        // \u{110000} out of range -> invalid
        assert_eq!(
            segments("\"\\u{110000}\""),
            vec![(0, 1, "string"), (1, 10, "invalid"), (11, 1, "string")]
        );
    }

    #[test]
    fn invalid_escape() {
        // \q is not a valid escape
        assert_eq!(
            segments("\"\\q\""),
            vec![(0, 1, "string"), (1, 2, "invalid"), (3, 1, "string")]
        );
        // trailing backslash
        assert_eq!(segments("\"\\"), vec![(0, 1, "string"), (1, 1, "invalid")]);
    }

    #[test]
    fn multibyte_literal_between_escapes() {
        // "é\n" — é is 2 bytes (U+00E9). Offsets must be byte-based.
        let s = "\"\u{e9}\\n\"";
        // bytes: 0:" 1-2:é 3:\ 4:n 5:"
        assert_eq!(
            segments(s),
            vec![(0, 3, "string"), (3, 2, "escape"), (5, 1, "string")]
        );
    }

    #[test]
    fn consecutive_escapes() {
        // "\n\t" -> `"`, `\n`, `\t`, `"`
        assert_eq!(
            segments("\"\\n\\t\""),
            vec![
                (0, 1, "string"),
                (1, 2, "escape"),
                (3, 2, "escape"),
                (5, 1, "string")
            ]
        );
    }
}
