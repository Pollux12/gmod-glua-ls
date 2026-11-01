use std::{collections::HashMap, fmt};

use lsp_types::{Position, Range, TextDocumentContentChangeEvent, Uri};

/// The text of each open document as the editor holds it, kept by applying
/// every `textDocument/didChange` in the order the client sent it.
#[derive(Default)]
pub struct EditorDocuments {
    /// `None` once a change could not be applied: the text no longer matches
    /// the editor's, so ranged changes are refused until the document reopens.
    texts: HashMap<Uri, Option<String>>,
}

impl EditorDocuments {
    pub fn open(&mut self, uri: Uri, text: String) {
        self.texts.insert(uri, Some(text));
    }

    pub fn close(&mut self, uri: &Uri) {
        self.texts.remove(uri);
    }

    /// Applies one notification's changes in order and returns the full text
    /// after them. On error nothing is returned and the document stays out of
    /// sync until it is reopened or a full-text change arrives.
    pub fn change(
        &mut self,
        uri: &Uri,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Result<String, SyncError> {
        let entry = self.texts.get_mut(uri);
        let missing = if entry.is_some() {
            SyncError::OutOfSync
        } else {
            SyncError::NotOpen
        };
        let mut text = entry.and_then(Option::take);
        for change in changes {
            match change.range {
                None => text = Some(change.text),
                Some(range) => {
                    let text = text.as_mut().ok_or_else(|| missing.clone())?;
                    apply_ranged_change(text, range, &change.text)?;
                }
            }
        }
        let text = text.ok_or(missing)?;
        self.texts.insert(uri.clone(), Some(text.clone()));
        Ok(text)
    }
}

/// Why a didChange could not be applied to the editor text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// A ranged change for a document that is not open.
    NotOpen,
    /// A ranged change after an earlier change could not be applied.
    OutOfSync,
    /// A range whose line is past the last line of the document.
    LineOutOfRange(Position),
    /// A range whose start comes after its end.
    InvertedRange(Range),
    /// A column between the two UTF-16 units of one character.
    SplitsCharacter(Position),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::NotOpen => write!(f, "ranged change for a document that is not open"),
            SyncError::OutOfSync => write!(f, "document is out of sync since an earlier change"),
            SyncError::LineOutOfRange(position) => {
                write!(f, "line {} is past the end of the document", position.line)
            }
            SyncError::InvertedRange(range) => write!(
                f,
                "range start {}:{} is after its end {}:{}",
                range.start.line, range.start.character, range.end.line, range.end.character
            ),
            SyncError::SplitsCharacter(position) => write!(
                f,
                "column {} on line {} splits a character",
                position.character, position.line
            ),
        }
    }
}

fn apply_ranged_change(text: &mut String, range: Range, new_text: &str) -> Result<(), SyncError> {
    let as_tuple = |position: Position| (position.line, position.character);
    if as_tuple(range.start) > as_tuple(range.end) {
        return Err(SyncError::InvertedRange(range));
    }
    let start_line = line_start(text, 0, 0, range.start)?;
    let start = column_offset(text, start_line, range.start)?;
    let end_line = line_start(text, start_line, range.start.line, range.end)?;
    let end = column_offset(text, end_line, range.end)?;
    text.replace_range(start..end, new_text);
    Ok(())
}

/// Byte offset where `position`'s line starts, scanning on from `from`, the
/// start of line `from_line` at or before it.
fn line_start(
    text: &str,
    from: usize,
    from_line: u32,
    position: Position,
) -> Result<usize, SyncError> {
    let mut start = from;
    for _ in from_line..position.line {
        let newline = text[start..]
            .find('\n')
            .ok_or(SyncError::LineOutOfRange(position))?;
        start += newline + 1;
    }
    Ok(start)
}

/// Byte offset of `position`'s UTF-16 column on the line starting at
/// `line_start`; a column past the line end means the line end.
fn column_offset(text: &str, line_start: usize, position: Position) -> Result<usize, SyncError> {
    let rest = &text[line_start..];
    let line = match rest.find('\n') {
        Some(end) => rest[..end].strip_suffix('\r').unwrap_or(&rest[..end]),
        None => rest,
    };
    let column = position.character as usize;
    let mut units = 0;
    for (offset, ch) in line.char_indices() {
        if units >= column {
            return if units == column {
                Ok(line_start + offset)
            } else {
                Err(SyncError::SplitsCharacter(position))
            };
        }
        units += ch.len_utf16();
    }
    if units > column {
        Err(SyncError::SplitsCharacter(position))
    } else {
        Ok(line_start + line.len())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use googletest::prelude::*;

    use super::*;

    fn uri() -> Uri {
        Uri::from_str("file:///workspace/lua/autorun/sync.lua").unwrap()
    }

    fn ranged(start: (u32, u32), end: (u32, u32), text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range::new(
                Position::new(start.0, start.1),
                Position::new(end.0, end.1),
            )),
            range_length: None,
            text: text.to_string(),
        }
    }

    fn full(text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_string(),
        }
    }

    fn opened(text: &str) -> EditorDocuments {
        let mut documents = EditorDocuments::default();
        documents.open(uri(), text.to_string());
        documents
    }

    /// Changes in one notification apply in order, each to the text the one
    /// before it produced.
    #[gtest]
    fn changes_in_one_notification_apply_in_order() -> Result<()> {
        let mut documents = opened("local a = 1\n");
        let text = documents.change(
            &uri(),
            vec![ranged((0, 6), (0, 7), "bb"), ranged((0, 7), (0, 8), "c")],
        );
        verify_that!(text, ok(eq("local bc = 1\n")))
    }

    /// A column past the line end means the line end, before the CRLF.
    #[gtest]
    fn a_column_past_the_line_end_clamps_before_the_line_break() -> Result<()> {
        let mut documents = opened("ab\r\ncd");
        let text = documents.change(&uri(), vec![ranged((0, 9), (0, 9), "!")]);
        verify_that!(text, ok(eq("ab!\r\ncd")))
    }

    /// Columns count UTF-16 units, so an emoji is two columns wide.
    #[gtest]
    fn columns_count_utf16_units() -> Result<()> {
        let mut documents = opened("x = \"😀é\"");
        let text = documents.change(&uri(), vec![ranged((0, 7), (0, 8), "e")]);
        verify_that!(text, ok(eq("x = \"😀e\"")))
    }

    /// A column inside a surrogate pair is refused instead of rounded.
    #[gtest]
    fn a_column_inside_a_surrogate_pair_is_refused() -> Result<()> {
        let mut documents = opened("😀");
        let text = documents.change(&uri(), vec![ranged((0, 1), (0, 1), "x")]);
        verify_that!(
            text,
            err(eq(&SyncError::SplitsCharacter(Position::new(0, 1))))
        )
    }

    /// A line past the end is refused, and later ranged changes are too,
    /// because the text no longer matches the editor's.
    #[gtest]
    fn a_bad_change_refuses_later_ranged_changes() -> Result<()> {
        let mut documents = opened("a\n");
        let bad = documents.change(&uri(), vec![ranged((5, 0), (5, 0), "x")]);
        expect_that!(
            bad,
            err(eq(&SyncError::LineOutOfRange(Position::new(5, 0))))
        );
        let later = documents.change(&uri(), vec![ranged((0, 0), (0, 0), "x")]);
        verify_that!(later, err(eq(&SyncError::OutOfSync)))
    }

    /// A full-text change brings an out-of-sync document back.
    #[gtest]
    fn a_full_text_change_resyncs_the_document() -> Result<()> {
        let mut documents = opened("a\n");
        let _ = documents.change(&uri(), vec![ranged((0, 1), (0, 0), "x")]);
        let text = documents.change(&uri(), vec![full("b\n"), ranged((0, 1), (0, 1), "c")]);
        verify_that!(text, ok(eq("bc\n")))
    }

    /// A notification without changes returns the current text.
    #[gtest]
    fn a_notification_without_changes_returns_the_current_text() -> Result<()> {
        let mut documents = opened("a\n");
        verify_that!(documents.change(&uri(), Vec::new()), ok(eq("a\n")))
    }

    /// A ranged change for a document that was never opened is refused.
    #[gtest]
    fn a_ranged_change_for_an_unopened_document_is_refused() -> Result<()> {
        let mut documents = EditorDocuments::default();
        let text = documents.change(&uri(), vec![ranged((0, 0), (0, 0), "x")]);
        verify_that!(text, err(eq(&SyncError::NotOpen)))
    }

    /// Deterministic xorshift generator for the randomized sync test.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    const PIECES: &[&str] = &[
        "a",
        "local x = 1",
        " ",
        "\n",
        "\r\n",
        "é",
        "日本",
        "😀",
        "🎉x",
        "\t",
        "--",
        "end",
        "",
    ];

    fn random_text(rng: &mut Rng) -> String {
        (0..rng.below(8))
            .map(|_| PIECES[rng.below(PIECES.len())])
            .collect()
    }

    /// Byte offsets the editor can address: char boundaries, except the one
    /// inside a CRLF.
    fn addressable_offsets(text: &str) -> Vec<usize> {
        (0..=text.len())
            .filter(|&i| text.is_char_boundary(i))
            .filter(|&i| !(text[..i].ends_with('\r') && text[i..].starts_with('\n')))
            .collect()
    }

    /// The editor's (line, UTF-16 column) of a byte offset, CRLF being one line break.
    fn editor_position(text: &str, offset: usize) -> Position {
        let before = &text[..offset];
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let line = before.matches('\n').count();
        let character = before[line_start..].encode_utf16().count();
        Position::new(line as u32, character as u32)
    }

    /// Random ranged edits, with multi-byte and surrogate-pair characters, CRLF
    /// line breaks, edits at line ends and EOF, and several changes per
    /// notification, leave the server text equal to the editor text after
    /// every notification.
    #[gtest]
    fn random_ranged_edits_keep_the_server_text_equal_to_the_editor_text() -> Result<()> {
        for seed in 1..=200u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut editor = random_text(&mut rng);
            let mut documents = opened(&editor);
            for version in 0..60 {
                let mut changes = Vec::new();
                for _ in 0..=rng.below(4) {
                    if rng.below(40) == 0 {
                        editor = random_text(&mut rng);
                        changes.push(full(&editor));
                        continue;
                    }
                    let offsets = addressable_offsets(&editor);
                    let mut start = offsets[rng.below(offsets.len())];
                    let mut end = match rng.below(4) {
                        0 => start,
                        1 => editor.len(),
                        _ => offsets[rng.below(offsets.len())],
                    };
                    if start > end {
                        std::mem::swap(&mut start, &mut end);
                    }
                    let mut range = Range::new(
                        editor_position(&editor, start),
                        editor_position(&editor, end),
                    );
                    let at_line_end =
                        end == editor.len() || editor[end..].starts_with(['\r', '\n']);
                    if rng.below(8) == 0 && at_line_end {
                        range.end.character += 1 + rng.below(3) as u32;
                    }
                    let inserted = random_text(&mut rng);
                    editor.replace_range(start..end, &inserted);
                    changes.push(TextDocumentContentChangeEvent {
                        range: Some(range),
                        range_length: None,
                        text: inserted,
                    });
                }
                let text = documents.change(&uri(), changes);
                verify_that!(text, ok(eq(&editor)))
                    .with_failure_message(|| format!("seed {seed}, notification {version}"))?;
            }
        }
        Ok(())
    }
}
