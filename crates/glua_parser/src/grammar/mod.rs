mod doc;
mod lua;

use crate::{parser::CompleteMarker, parser_error::LuaParseError};
pub use doc::parse_comment;
pub use lua::parse_chunk;

type ParseResult = Result<CompleteMarker, ParseFailReason>;
type DocParseResult = Result<CompleteMarker, LuaParseError>;

/// Minimum stack bytes that must remain to keep parsing nested constructs.
///
/// Source-controlled nesting (expressions, blocks) recurses per level and can
/// exhaust small worker stacks on generated files. Callers bail with a syntax
/// error — the same control flow as a genuine syntax error at that position,
/// so existing recovery terminates exactly as it does for malformed input —
/// instead of aborting the process.
///
/// Fail closed: a `None` query result (unsupported target / OS query failure)
/// counts as exhausted rather than safe, mirroring how `stacker::maybe_grow`
/// treats `None` as insufficient.
pub(crate) fn parser_stack_exhausted() -> bool {
    match stacker::remaining_stack() {
        Some(remaining) => remaining < 256 * 1024,
        None => true,
    }
}
pub enum ParseFailReason {
    /// Parsing was stopped due to reaching the end of the file.
    Eof,
    /// Parsing was stopped due to encountering an unexpected token.
    UnexpectedToken,
}
