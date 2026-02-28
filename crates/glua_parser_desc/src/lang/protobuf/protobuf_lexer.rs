use glua_parser::{LexerState, Reader, SourceRange};

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtobufTokenKind {
    TkEof,
    TkEndOfLine,
    TkWhitespace,
    TkLineComment,
    TkBlockComment,
    TkString,
    TkNumber,
    TkFloat,
    TkKeyword, // syntax, package, import, message, service, etc.
    TkType,    // int32, string, bool, etc.
    TkIdentifier,
    TkSemicolon,
    TkComma,
    TkDot,
    TkEquals,
    TkLeftParen,
    TkRightParen,
    TkLeftBrace,
    TkRightBrace,
    TkLeftBracket,
    TkRightBracket,
    TkLeftAngle,
    TkRightAngle,
    TkColon,
    TkSlash,
    TkMinus,
    TkPlus,
    TkUnknown,
}

#[derive(Debug)]
pub struct ProtobufTokenData {
    pub kind: ProtobufTokenKind,
    pub range: SourceRange,
}

impl ProtobufTokenData {
    pub fn new(kind: ProtobufTokenKind, range: SourceRange) -> Self {
        Self { kind, range }
    }
}

#[derive(Debug)]
pub struct ProtobufLexer<'a> {
    reader: Reader<'a>,
    state: LexerState,
}

impl<'a> ProtobufLexer<'a> {
    pub fn new_with_state(reader: Reader<'a>, state: LexerState) -> Self {
        Self { reader, state }
    }

    pub fn tokenize(&mut self) -> Vec<ProtobufTokenData> {
        let mut tokens = vec![];

        while !self.reader.is_eof() {
            let kind = match self.state {
                LexerState::Normal => self.lex(),
                LexerState::String(quote) => self.lex_string(quote),
                _ => ProtobufTokenKind::TkUnknown,
            };

            if kind == ProtobufTokenKind::TkEof {
                break;
            }

            tokens.push(ProtobufTokenData::new(kind, self.reader.current_range()));
        }

        tokens
    }

    pub fn get_state(&self) -> LexerState {
        self.state
    }

    fn lex(&mut self) -> ProtobufTokenKind {
        self.reader.reset_buff();

        match self.reader.current_char() {
            '\n' | '\r' => self.lex_newline(),
            ' ' | '\t' => self.lex_whitespace(),
            '/' => self.lex_comment_or_slash(),
            '"' => {
                let quote = self.reader.current_char();
                self.reader.bump();
                self.state = LexerState::String(quote);
                self.lex_string(quote)
            }
            '\'' => {
                let quote = self.reader.current_char();
                self.reader.bump();
                self.state = LexerState::String(quote);
                self.lex_string(quote)
            }
            '0'..='9' => self.lex_number(),
            ';' => {
                self.reader.bump();
                ProtobufTokenKind::TkSemicolon
            }
            ',' => {
                self.reader.bump();
                ProtobufTokenKind::TkComma
            }
            '.' => {
                self.reader.bump();
                ProtobufTokenKind::TkDot
            }
            '=' => {
                self.reader.bump();
                ProtobufTokenKind::TkEquals
            }
            '(' => {
                self.reader.bump();
                ProtobufTokenKind::TkLeftParen
            }
            ')' => {
                self.reader.bump();
                ProtobufTokenKind::TkRightParen
            }
            '{' => {
                self.reader.bump();
                ProtobufTokenKind::TkLeftBrace
            }
            '}' => {
                self.reader.bump();
                ProtobufTokenKind::TkRightBrace
            }
            '[' => {
                self.reader.bump();
                ProtobufTokenKind::TkLeftBracket
            }
            ']' => {
                self.reader.bump();
                ProtobufTokenKind::TkRightBracket
            }
            '<' => {
                self.reader.bump();
                ProtobufTokenKind::TkLeftAngle
            }
            '>' => {
                self.reader.bump();
                ProtobufTokenKind::TkRightAngle
            }
            ':' => {
                self.reader.bump();
                ProtobufTokenKind::TkColon
            }
            '-' => {
                self.reader.bump();
                ProtobufTokenKind::TkMinus
            }
            '+' => {
                self.reader.bump();
                ProtobufTokenKind::TkPlus
            }
            _ if self.reader.is_eof() => ProtobufTokenKind::TkEof,
            ch if is_identifier_start(ch) => self.lex_identifier(),
            _ => {
                self.reader.bump();
                ProtobufTokenKind::TkUnknown
            }
        }
    }

    fn lex_newline(&mut self) -> ProtobufTokenKind {
        if self.reader.current_char() == '\r' {
            self.reader.bump();
            if self.reader.current_char() == '\n' {
                self.reader.bump();
            }
        } else {
            self.reader.bump();
        }
        ProtobufTokenKind::TkEndOfLine
    }

    fn lex_whitespace(&mut self) -> ProtobufTokenKind {
        self.reader.eat_while(|ch| ch == ' ' || ch == '\t');
        ProtobufTokenKind::TkWhitespace
    }

    fn lex_comment_or_slash(&mut self) -> ProtobufTokenKind {
        self.reader.bump(); // consume '/'

        match self.reader.current_char() {
            '/' => {
                // Line comment
                self.reader.bump();
                self.reader.eat_while(|ch| ch != '\n' && ch != '\r');
                ProtobufTokenKind::TkLineComment
            }
            '*' => {
                // Block comment
                self.reader.bump();
                while !self.reader.is_eof() {
                    if self.reader.current_char() == '*' {
                        self.reader.bump();
                        if self.reader.current_char() == '/' {
                            self.reader.bump();
                            break;
                        }
                    } else {
                        self.reader.bump();
                    }
                }
                ProtobufTokenKind::TkBlockComment
            }
            _ => ProtobufTokenKind::TkSlash,
        }
    }

    fn lex_string(&mut self, quote: char) -> ProtobufTokenKind {
        while !self.reader.is_eof() {
            let ch = self.reader.current_char();
            if ch == quote {
                break;
            }
            if ch == '\\' {
                self.reader.bump(); // consume backslash
                if !self.reader.is_eof() {
                    self.reader.bump(); // consume escaped character
                }
            } else {
                self.reader.bump();
            }
        }

        if self.reader.current_char() == quote {
            self.reader.bump();
            self.state = LexerState::Normal;
        }

        ProtobufTokenKind::TkString
    }

    fn lex_number(&mut self) -> ProtobufTokenKind {
        self.reader.eat_while(|ch| ch.is_ascii_digit());

        if self.reader.current_char() == '.' && self.reader.next_char().is_ascii_digit() {
            self.reader.bump(); // consume '.'
            self.reader.eat_while(|ch| ch.is_ascii_digit());

            // Handle scientific notation
            if matches!(self.reader.current_char(), 'e' | 'E') {
                self.reader.bump();
                if matches!(self.reader.current_char(), '+' | '-') {
                    self.reader.bump();
                }
                self.reader.eat_while(|ch| ch.is_ascii_digit());
            }

            ProtobufTokenKind::TkFloat
        } else {
            ProtobufTokenKind::TkNumber
        }
    }

    fn lex_identifier(&mut self) -> ProtobufTokenKind {
        self.reader.eat_while(is_identifier_continue);
        let text = self.reader.current_text();

        // Check if it's a keyword
        match text {
            "syntax" | "package" | "import" | "option" | "message" | "service" | "rpc" | "enum"
            | "oneof" | "repeated" | "optional" | "required" | "reserved" | "extensions"
            | "extend" | "group" | "stream" | "returns" | "public" | "weak" | "map" => {
                ProtobufTokenKind::TkKeyword
            }

            // Built-in types
            "double" | "float" | "int32" | "int64" | "uint32" | "uint64" | "sint32" | "sint64"
            | "fixed32" | "fixed64" | "sfixed32" | "sfixed64" | "bool" | "string" | "bytes" => {
                ProtobufTokenKind::TkType
            }

            _ => ProtobufTokenKind::TkIdentifier,
        }
    }
}

fn is_identifier_start(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_'
}

fn is_identifier_continue(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use glua_parser::Reader;

    #[test]
    fn test_simple_message() {
        let input = r#"syntax = "proto3";

message Person {
    string name = 1;
    int32 age = 2;
}
"#;
        let reader = Reader::new(input);
        let mut lexer = ProtobufLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        // Just check that we can tokenize without panicking
        assert!(!tokens.is_empty());

        // Check some specific tokens
        let keyword_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == ProtobufTokenKind::TkKeyword)
            .collect();
        assert!(keyword_tokens.len() >= 2); // "syntax" and "message"
    }

    #[test]
    fn test_comments() {
        let input = r#"// Line comment
/* Block comment */
syntax = "proto3";"#;
        let reader = Reader::new(input);
        let mut lexer = ProtobufLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        let comment_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    ProtobufTokenKind::TkLineComment | ProtobufTokenKind::TkBlockComment
                )
            })
            .collect();
        assert_eq!(comment_tokens.len(), 2);
    }

    #[test]
    fn test_numbers() {
        let input = "123 456.789 1.5e-10";
        let reader = Reader::new(input);
        let mut lexer = ProtobufLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        let number_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    ProtobufTokenKind::TkNumber | ProtobufTokenKind::TkFloat
                )
            })
            .collect();
        assert_eq!(number_tokens.len(), 3);
    }

    #[test]
    fn test_complex_protobuf() {
        let input = r#"syntax = "proto3";

package com.example;

import "google/protobuf/timestamp.proto";

// Service definition
service UserService {
    rpc GetUser(GetUserRequest) returns (User);
    rpc CreateUser(CreateUserRequest) returns (User);
}

message User {
    int64 id = 1;
    string name = 2;
    string email = 3;
    repeated string tags = 4;
    google.protobuf.Timestamp created_at = 5;

    enum Status {
        UNKNOWN = 0;
        ACTIVE = 1;
        INACTIVE = 2;
    }
    Status status = 6;

    oneof contact {
        string phone = 7;
        string address = 8;
    }
}

message GetUserRequest {
    int64 user_id = 1;
}

message CreateUserRequest {
    string name = 1;
    string email = 2;
}"#;
        let reader = Reader::new(input);
        let mut lexer = ProtobufLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        // Check we have keywords
        let keyword_count = tokens
            .iter()
            .filter(|t| t.kind == ProtobufTokenKind::TkKeyword)
            .count();
        assert!(keyword_count > 10); // Many keywords in this example

        // Check we have types
        let type_count = tokens
            .iter()
            .filter(|t| t.kind == ProtobufTokenKind::TkType)
            .count();
        assert!(type_count > 5); // Several built-in types

        // Check we have identifiers
        let identifier_count = tokens
            .iter()
            .filter(|t| t.kind == ProtobufTokenKind::TkIdentifier)
            .count();
        assert!(identifier_count > 15); // User-defined names

        // Check we have numbers
        let number_count = tokens
            .iter()
            .filter(|t| {
                matches!(
                    t.kind,
                    ProtobufTokenKind::TkNumber | ProtobufTokenKind::TkFloat
                )
            })
            .count();
        assert!(number_count >= 8); // Field numbers

        // Check we have strings
        let string_count = tokens
            .iter()
            .filter(|t| t.kind == ProtobufTokenKind::TkString)
            .count();
        assert_eq!(string_count, 2); // "proto3" and import path
    }

    #[test]
    fn test_string_escaping() {
        let input = r#""hello \"world\" \n""#;
        let reader = Reader::new(input);
        let mut lexer = ProtobufLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        let string_tokens: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == ProtobufTokenKind::TkString)
            .collect();
        assert_eq!(string_tokens.len(), 1);
    }
}
