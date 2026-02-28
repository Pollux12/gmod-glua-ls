mod doc;
mod lua;

use crate::{parser::CompleteMarker, parser_error::LuaParseError};
pub use doc::parse_comment;
pub use lua::parse_chunk;

type ParseResult = Result<CompleteMarker, ParseFailReason>;
type DocParseResult = Result<CompleteMarker, LuaParseError>;
pub enum ParseFailReason {
    /// Parsing was stopped due to reaching the end of the file.
    Eof,
    /// Parsing was stopped due to encountering an unexpected token.
    UnexpectedToken,
}
