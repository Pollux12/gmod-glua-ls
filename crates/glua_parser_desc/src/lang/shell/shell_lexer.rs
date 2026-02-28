use glua_parser::{LexerState, Reader, SourceRange};

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellTokenKind {
    TkEof,
    TkEndOfLine,
    TkWhitespace,
    TkComment,
    TkString,
    TkSingleQuotedString,
    TkDoubleQuotedString,
    TkBacktickString,
    TkHereDoc,
    TkNumber,
    TkKeyword,
    TkBuiltin,
    TkCommand,
    TkVariable,
    TkOperator,
    TkRedirection,
    TkPipe,
    TkBackground,
    TkSemicolon,
    TkAnd,
    TkOr,
    TkLeftParen,
    TkRightParen,
    TkLeftBrace,
    TkRightBrace,
    TkLeftBracket,
    TkRightBracket,
    TkDollar,
    TkUnknown,
}

#[derive(Debug)]
pub struct ShellTokenData {
    pub kind: ShellTokenKind,
    pub range: SourceRange,
}

impl ShellTokenData {
    pub fn new(kind: ShellTokenKind, range: SourceRange) -> Self {
        Self { kind, range }
    }
}

#[derive(Debug)]
pub struct ShellLexer<'a> {
    reader: Reader<'a>,
    state: LexerState,
}

impl<'a> ShellLexer<'a> {
    pub fn new_with_state(reader: Reader<'a>, state: LexerState) -> Self {
        ShellLexer { reader, state }
    }

    pub fn tokenize(&mut self) -> Vec<ShellTokenData> {
        let mut tokens = vec![];

        while !self.reader.is_eof() {
            let kind = match self.state {
                LexerState::Normal => self.lex(),
                LexerState::String(quote) => self.lex_string(quote),
                _ => ShellTokenKind::TkUnknown,
            };

            if kind == ShellTokenKind::TkEof {
                break;
            }

            tokens.push(ShellTokenData::new(kind, self.reader.current_range()));
        }

        tokens
    }

    pub fn get_state(&self) -> LexerState {
        self.state
    }

    fn lex(&mut self) -> ShellTokenKind {
        self.reader.reset_buff();

        match self.reader.current_char() {
            '\n' | '\r' => self.lex_new_line(),
            ' ' | '\t' => self.lex_whitespace(),
            '#' => self.lex_comment(),
            '\'' => {
                let quote = self.reader.current_char();
                self.reader.bump();
                self.state = LexerState::String(quote);
                self.lex_string(quote)
            }
            '"' => {
                let quote = self.reader.current_char();
                self.reader.bump();
                self.state = LexerState::String(quote);
                self.lex_string(quote)
            }
            '`' => {
                let quote = self.reader.current_char();
                self.reader.bump();
                self.state = LexerState::String(quote);
                self.lex_string(quote)
            }
            '0'..='9' => self.lex_number(),
            'a'..='z' | 'A'..='Z' | '_' => self.lex_identifier(),
            '$' => self.lex_variable(),
            '|' => {
                self.reader.bump();
                if self.reader.current_char() == '|' {
                    self.reader.bump();
                    ShellTokenKind::TkOr
                } else {
                    ShellTokenKind::TkPipe
                }
            }
            '&' => {
                self.reader.bump();
                if self.reader.current_char() == '&' {
                    self.reader.bump();
                    ShellTokenKind::TkAnd
                } else {
                    ShellTokenKind::TkBackground
                }
            }
            '>' => {
                self.reader.bump();
                if self.reader.current_char() == '>' {
                    self.reader.bump();
                }
                ShellTokenKind::TkRedirection
            }
            '<' => {
                self.reader.bump();
                if self.reader.current_char() == '<' {
                    self.reader.bump();
                    if self.reader.current_char() == '<' {
                        self.reader.bump();
                        return self.lex_heredoc();
                    }
                }
                ShellTokenKind::TkRedirection
            }
            ';' => {
                self.reader.bump();
                ShellTokenKind::TkSemicolon
            }
            '(' => {
                self.reader.bump();
                ShellTokenKind::TkLeftParen
            }
            ')' => {
                self.reader.bump();
                ShellTokenKind::TkRightParen
            }
            '{' => {
                self.reader.bump();
                ShellTokenKind::TkLeftBrace
            }
            '}' => {
                self.reader.bump();
                ShellTokenKind::TkRightBrace
            }
            '[' => {
                self.reader.bump();
                ShellTokenKind::TkLeftBracket
            }
            ']' => {
                self.reader.bump();
                ShellTokenKind::TkRightBracket
            }
            '+' | '-' | '*' | '/' | '%' | '=' | '!' | '^' | '~' | ',' | '.' | ':' => {
                self.reader.bump();
                ShellTokenKind::TkOperator
            }
            _ if self.reader.is_eof() => ShellTokenKind::TkEof,
            _ => {
                self.reader.bump();
                ShellTokenKind::TkUnknown
            }
        }
    }

    fn lex_new_line(&mut self) -> ShellTokenKind {
        if self.reader.current_char() == '\r' {
            self.reader.bump();
        }
        if self.reader.current_char() == '\n' {
            self.reader.bump();
        }
        ShellTokenKind::TkEndOfLine
    }

    fn lex_whitespace(&mut self) -> ShellTokenKind {
        self.reader.eat_while(|c| c == ' ' || c == '\t');
        ShellTokenKind::TkWhitespace
    }

    fn lex_comment(&mut self) -> ShellTokenKind {
        if self.reader.current_char() == '#' {
            self.reader.bump();
            self.reader.eat_while(|ch| ch != '\n' && ch != '\r');
            ShellTokenKind::TkComment
        } else {
            self.reader.bump();
            ShellTokenKind::TkUnknown
        }
    }

    fn lex_string(&mut self, quote: char) -> ShellTokenKind {
        let token_kind = match quote {
            '\'' => ShellTokenKind::TkSingleQuotedString,
            '"' => ShellTokenKind::TkDoubleQuotedString,
            '`' => ShellTokenKind::TkBacktickString,
            _ => ShellTokenKind::TkString,
        };

        while !self.reader.is_eof() {
            let ch = self.reader.current_char();
            if ch == quote {
                break;
            }

            if ch == '\n' || ch == '\r' {
                // Unterminated string
                break;
            }

            if ch == '\\' && quote != '\'' {
                // Handle escape sequences (except in single quotes)
                self.reader.bump();
                if !self.reader.is_eof() {
                    self.reader.bump();
                }
            } else {
                self.reader.bump();
            }
        }

        if self.reader.current_char() == quote {
            self.reader.bump();
            self.state = LexerState::Normal;
        }

        token_kind
    }

    fn lex_number(&mut self) -> ShellTokenKind {
        self.reader.eat_while(|c| c.is_ascii_digit());

        // Check for decimal numbers
        if self.reader.current_char() == '.' && self.reader.next_char().is_ascii_digit() {
            self.reader.bump(); // consume '.'
            self.reader.eat_while(|c| c.is_ascii_digit());
        }

        // Check for scientific notation
        if self.reader.current_char() == 'e' || self.reader.current_char() == 'E' {
            self.reader.bump();
            if self.reader.current_char() == '+' || self.reader.current_char() == '-' {
                self.reader.bump();
            }
            self.reader.eat_while(|c| c.is_ascii_digit());
        }

        ShellTokenKind::TkNumber
    }

    fn lex_identifier(&mut self) -> ShellTokenKind {
        self.reader
            .eat_while(|c| c.is_alphanumeric() || c == '_' || c == '-');
        let text = self.reader.current_text();

        // Check if it's a shell keyword
        match text {
            "if" | "then" | "else" | "elif" | "fi" | "case" | "esac" | "for" | "while"
            | "until" | "do" | "done" | "function" | "select" | "time" | "in" | "export"
            | "local" | "readonly" | "declare" | "typeset" | "return" | "exit" | "break"
            | "continue" | "shift" | "eval" | "exec" | "source" | "alias" | "unalias"
            | "history" | "fc" | "jobs" | "bg" | "fg" | "disown" | "suspend" | "wait" | "trap"
            | "ulimit" | "umask" | "set" | "shopt" | "enable" | "disable" | "builtin"
            | "command" | "type" | "hash" | "help" | "bind" | "complete" | "compgen" | "read"
            | "mapfile" | "readarray" | "caller" | "dirs" | "pushd" | "popd" => {
                ShellTokenKind::TkKeyword
            }

            // Common shell builtins
            "echo" | "printf" | "test" | "true" | "false" | "cd" | "pwd" | "ls" | "cat"
            | "grep" | "awk" | "sed" | "sort" | "uniq" | "cut" | "tr" | "head" | "tail" | "wc"
            | "find" | "xargs" | "ps" | "killall" | "top" | "htop" | "df" | "du" | "mount"
            | "umount" | "chmod" | "chown" | "chgrp" | "cp" | "mv" | "rm" | "mkdir" | "rmdir"
            | "touch" | "ln" | "file" | "locate" | "updatedb" | "tar" | "gzip" | "gunzip"
            | "zip" | "unzip" | "curl" | "wget" | "ssh" | "scp" | "rsync" | "git" | "svn"
            | "make" | "gcc" | "g++" | "python" | "python3" | "node" | "npm" | "vim" | "nano"
            | "emacs" | "less" | "more" | "man" | "info" | "crontab" => ShellTokenKind::TkBuiltin,

            _ => ShellTokenKind::TkCommand,
        }
    }

    fn lex_variable(&mut self) -> ShellTokenKind {
        if self.reader.current_char() == '$' {
            self.reader.bump();

            // Handle special variable forms
            match self.reader.current_char() {
                '{' => {
                    // ${variable} form
                    self.reader.bump();
                    self.reader.eat_while(|c| {
                        c.is_alphanumeric()
                            || c == '_'
                            || c == '@'
                            || c == '*'
                            || c == '?'
                            || c == '#'
                            || c == '!'
                            || c == '-'
                            || c == '='
                            || c == ':'
                    });
                    if self.reader.current_char() == '}' {
                        self.reader.bump();
                    }
                }
                '(' => {
                    // $(command) form
                    self.reader.bump();
                    let mut paren_count = 1;
                    while !self.reader.is_eof() && paren_count > 0 {
                        match self.reader.current_char() {
                            '(' => paren_count += 1,
                            ')' => paren_count -= 1,
                            _ => {}
                        }
                        self.reader.bump();
                    }
                }
                '0'..='9' | '@' | '*' | '?' | '#' | '!' | '$' | '-' => {
                    // Special variables like $1, $@, $*, $?, $#, $!, $$, $-
                    self.reader.bump();
                }
                'a'..='z' | 'A'..='Z' | '_' => {
                    // Regular variable name
                    self.reader.eat_while(|c| c.is_alphanumeric() || c == '_');
                }
                _ => {
                    // Just a $ symbol
                    return ShellTokenKind::TkDollar;
                }
            }

            ShellTokenKind::TkVariable
        } else {
            self.reader.bump();
            ShellTokenKind::TkDollar
        }
    }

    fn lex_heredoc(&mut self) -> ShellTokenKind {
        // This is a simplified heredoc lexer
        // In a real implementation, you'd need to handle the delimiter properly
        self.reader.eat_while(|ch| ch != '\n' && ch != '\r');
        ShellTokenKind::TkHereDoc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glua_parser::Reader;

    #[test]
    fn test_shell_lexer_basic() {
        let shell_script = r#"#!/bin/bash
# This is a comment
echo "Hello, World!"
ls -la /home
if [ -f file.txt ]; then
    cat file.txt
fi"#;

        let reader = Reader::new(shell_script);
        let mut lexer = ShellLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        // Count different token types
        let mut keyword_count = 0;
        let mut builtin_count = 0;
        let mut string_count = 0;
        let mut comment_count = 0;

        for token in &tokens {
            match token.kind {
                ShellTokenKind::TkKeyword => keyword_count += 1,
                ShellTokenKind::TkBuiltin => builtin_count += 1,
                ShellTokenKind::TkDoubleQuotedString => string_count += 1,
                ShellTokenKind::TkComment => comment_count += 1,
                _ => {}
            }
        }

        assert!(keyword_count > 0, "Should find keywords");
        assert!(builtin_count > 0, "Should find builtins");
        assert!(string_count > 0, "Should find strings");
        assert!(comment_count > 0, "Should find comments");
    }

    #[test]
    fn test_shell_lexer_variables() {
        let shell_script = r#"$HOME ${USER} $1 $@ $* $? $# $! $$ $-"#;

        let reader = Reader::new(shell_script);
        let mut lexer = ShellLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        let variables: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkVariable)
            .collect();

        assert!(variables.len() >= 5, "Should find multiple variables");
    }

    #[test]
    fn test_shell_lexer_strings() {
        let shell_script = r#"'single quote' "double quote" `backtick command`"#;

        let reader = Reader::new(shell_script);
        let mut lexer = ShellLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        let single_quotes = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkSingleQuotedString)
            .count();
        let double_quotes = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkDoubleQuotedString)
            .count();
        let backticks = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkBacktickString)
            .count();

        assert_eq!(single_quotes, 1, "Should find single quoted string");
        assert_eq!(double_quotes, 1, "Should find double quoted string");
        assert_eq!(backticks, 1, "Should find backtick string");
    }

    #[test]
    fn test_shell_lexer_operators() {
        let shell_script = r#"cmd1 | cmd2 && cmd3 || cmd4 > file < input >> append"#;

        let reader = Reader::new(shell_script);
        let mut lexer = ShellLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        let pipes = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkPipe)
            .count();
        let ands = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkAnd)
            .count();
        let ors = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkOr)
            .count();
        let redirections = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkRedirection)
            .count();

        assert_eq!(pipes, 1, "Should find pipe operator");
        assert_eq!(ands, 1, "Should find && operator");
        assert_eq!(ors, 1, "Should find || operator");
        assert!(redirections >= 3, "Should find redirection operators");
    }

    #[test]
    fn test_shell_lexer_keywords() {
        let shell_script = r#"if [ condition ]; then
    echo "true"
else
    echo "false"
fi

for item in list; do
    echo $item
done"#;

        let reader = Reader::new(shell_script);
        let mut lexer = ShellLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        let keywords: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkKeyword)
            .collect();

        assert!(keywords.len() >= 6, "Should find multiple keywords");
    }

    #[test]
    fn test_shell_lexer_comments() {
        let shell_script = r#"#!/bin/bash
# This is a comment
echo "not a comment" # inline comment
# Another comment"#;

        let reader = Reader::new(shell_script);
        let mut lexer = ShellLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        let comments: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkComment)
            .collect();

        assert!(comments.len() >= 3, "Should find multiple comments");
    }

    #[test]
    fn test_shell_lexer_numbers() {
        let shell_script = r#"echo 42 3.14 1e10 0xFF"#;

        let reader = Reader::new(shell_script);
        let mut lexer = ShellLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        let numbers: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkNumber)
            .collect();

        assert!(numbers.len() >= 2, "Should find numbers");
    }

    #[test]
    fn test_shell_lexer_brackets_and_braces() {
        let shell_script = r#"array[0]="value" { echo "block"; } (subshell)"#;

        let reader = Reader::new(shell_script);
        let mut lexer = ShellLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        let left_brackets = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkLeftBracket)
            .count();
        let right_brackets = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkRightBracket)
            .count();
        let left_braces = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkLeftBrace)
            .count();
        let right_braces = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkRightBrace)
            .count();
        let left_parens = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkLeftParen)
            .count();
        let right_parens = tokens
            .iter()
            .filter(|t| t.kind == ShellTokenKind::TkRightParen)
            .count();

        assert_eq!(left_brackets, 1, "Should find left bracket");
        assert_eq!(right_brackets, 1, "Should find right bracket");
        assert_eq!(left_braces, 1, "Should find left brace");
        assert_eq!(right_braces, 1, "Should find right brace");
        assert_eq!(left_parens, 1, "Should find left paren");
        assert_eq!(right_parens, 1, "Should find right paren");
    }

    #[test]
    fn test_shell_lexer_comprehensive_demo() {
        let shell_script = r#"#!/bin/bash
# Advanced shell script demonstration
set -euo pipefail

# Variables and parameters
USER_HOME="${HOME:-/tmp}"
LOG_FILE="/var/log/script.log"
COUNTER=0

# Function definition
function log_message() {
    local message="$1"
    echo "$(date): $message" >> "$LOG_FILE"
}

# Conditional and loops
if [[ -n "$USER_HOME" ]]; then
    cd "$USER_HOME"

    for file in *.txt; do
        if [[ -f "$file" ]]; then
            # Command substitution and pipes
            file_size=$(stat -c%s "$file")
            echo "Processing: $file (size: $file_size bytes)" | tee -a "$LOG_FILE"
        fi
    done
fi

# Background processes and job control
long_running_task &
PID=$!
wait $PID"#;

        let reader = Reader::new(shell_script);
        let mut lexer = ShellLexer::new_with_state(reader, LexerState::Normal);
        let tokens = lexer.tokenize();

        // Count different token types
        let mut keyword_count = 0;
        let mut builtin_count = 0;
        let mut variable_count = 0;
        let mut comment_count = 0;

        for token in &tokens {
            match token.kind {
                ShellTokenKind::TkKeyword => keyword_count += 1,
                ShellTokenKind::TkBuiltin => builtin_count += 1,
                ShellTokenKind::TkVariable => variable_count += 1,
                ShellTokenKind::TkComment => comment_count += 1,
                _ => {}
            }
        }

        println!("Shell lexer found:");
        println!("  Keywords: {}", keyword_count);
        println!("  Builtins: {}", builtin_count);
        println!("  Variables: {}", variable_count);
        println!("  Comments: {}", comment_count);

        // Basic assertions
        assert!(keyword_count > 5, "Should find multiple keywords");
        assert!(builtin_count > 3, "Should find multiple builtins");
        assert!(variable_count >= 3, "Should find multiple variables");
        assert!(comment_count > 3, "Should find multiple comments");
    }
}
