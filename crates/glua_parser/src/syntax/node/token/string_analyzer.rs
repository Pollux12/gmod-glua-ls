use crate::{
    LuaKind, LuaSyntaxToken,
    kind::LuaTokenKind,
    parser_error::{LuaParseError, LuaParseErrorKind},
};

pub fn string_token_value(token: &LuaSyntaxToken) -> Result<String, LuaParseError> {
    match token.kind() {
        LuaKind::Token(LuaTokenKind::TkString) => normal_string_value(token),
        LuaKind::Token(LuaTokenKind::TkLongString) => long_string_value(token),
        _ => unreachable!(),
    }
}

fn long_string_value(token: &LuaSyntaxToken) -> Result<String, LuaParseError> {
    let range = token.text_range();
    let text = token.text();
    if text.len() < 4 {
        return Err(LuaParseError::new(
            LuaParseErrorKind::SyntaxError,
            &t!("String too short"),
            range,
        ));
    }

    let mut equal_num = 0;
    let mut i = 0;
    let mut chars = text.char_indices();

    // check first char
    if let Some((_, first_char)) = chars.next() {
        if first_char != '[' {
            return Err(LuaParseError::new(
                LuaParseErrorKind::SyntaxError,
                &t!(
                    "Invalid long string start, expected '[', found '%{char}'",
                    char = first_char
                ),
                range,
            ));
        }
    } else {
        return Err(LuaParseError::new(
            LuaParseErrorKind::SyntaxError,
            &t!("Invalid long string start, expected '[', found end of input"),
            range,
        ));
    }

    for (idx, c) in chars.by_ref() {
        // calc eq num
        if c == '=' {
            equal_num += 1;
        } else if c == '[' {
            i = idx + 1;
            break;
        } else {
            return Err(LuaParseError::new(
                LuaParseErrorKind::SyntaxError,
                &t!("Invalid long string start"),
                range,
            ));
        }
    }

    // check string len is enough
    if text.len() < i + equal_num + 2 {
        return Err(LuaParseError::new(
            LuaParseErrorKind::SyntaxError,
            &t!(
                "Invalid long string end, expected '%{eq}]'",
                eq = "=".repeat(equal_num)
            ),
            range,
        ));
    }

    // lua special rule for long string
    if let Some((_, first_content_char)) = chars.next() {
        if first_content_char == '\r' {
            if let Some((_, next_char)) = chars.next() {
                if next_char == '\n' {
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if first_content_char == '\n' {
            i += 1;
        }
    }

    let content = &text[i..(text.len() - equal_num - 2)];

    Ok(content.to_string())
}

fn normal_string_value(token: &LuaSyntaxToken) -> Result<String, LuaParseError> {
    let text = token.text();
    if text.len() < 2 {
        return Ok(String::new());
    }

    let mut result = String::with_capacity(text.len() - 2);
    let mut chars = text.chars().peekable();
    let delimiter = chars.next().unwrap();

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next_char) = chars.next() {
                    match next_char {
                        'a' => result.push('\u{0007}'), // Bell
                        'b' => result.push('\u{0008}'), // Backspace
                        'f' => result.push('\u{000C}'), // Formfeed
                        'n' => result.push('\n'),       // Newline
                        'r' => result.push('\r'),       // Carriage return
                        't' => result.push('\t'),       // Horizontal tab
                        'v' => result.push('\u{000B}'), // Vertical tab
                        'x' => {
                            // Hexadecimal escape sequence
                            let hex = chars.by_ref().take(2).collect::<String>();
                            if hex.len() == 2 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                                if let Ok(value) = u8::from_str_radix(&hex, 16) {
                                    result.push(value as char);
                                }
                            } else {
                                return Err(LuaParseError::new(
                                    LuaParseErrorKind::SyntaxError,
                                    &t!("Invalid hex escape sequence '\\x%{hex}'", hex = hex),
                                    token.text_range(),
                                ));
                            }
                        }
                        'u' => {
                            // Unicode escape sequence
                            if let Some('{') = chars.next() {
                                let unicode_hex =
                                    chars.by_ref().take_while(|c| *c != '}').collect::<String>();
                                if let Ok(code_point) = u32::from_str_radix(&unicode_hex, 16) {
                                    if let Some(unicode_char) = std::char::from_u32(code_point) {
                                        result.push(unicode_char);
                                    } else {
                                        return Err(LuaParseError::new(
                                            LuaParseErrorKind::SyntaxError,
                                            &t!(
                                                "Invalid unicode escape sequence '\\u{{%{unicode_hex}}}'",
                                                unicode_hex = unicode_hex
                                            ),
                                            token.text_range(),
                                        ));
                                    }
                                }
                            }
                        }
                        '0'..='9' => {
                            // Decimal escape sequence
                            let mut dec = String::new();
                            dec.push(next_char);
                            for _ in 0..2 {
                                if let Some(digit) = chars.peek() {
                                    if digit.is_ascii_digit() {
                                        dec.push(*digit);
                                    } else {
                                        break;
                                    }
                                    chars.next();
                                }
                            }
                            if let Ok(value) = dec.parse::<u8>() {
                                result.push(value as char);
                            }
                        }
                        '\\' | '\'' | '\"' => result.push(next_char),
                        'z' => {
                            // Skip whitespace
                            while let Some(c) = chars.peek() {
                                if !c.is_whitespace() {
                                    break;
                                }
                                chars.next();
                            }
                        }
                        '\r' | '\n' => {
                            result.push(next_char);
                        }
                        _ => {
                            return Err(LuaParseError::new(
                                LuaParseErrorKind::SyntaxError,
                                &t!("Invalid escape sequence '\\%{char}'", char = next_char),
                                token.text_range(),
                            ));
                        }
                    }
                }
            }
            _ => {
                if c == delimiter {
                    break;
                }
                result.push(c);
            }
        }
    }

    Ok(result)
}
