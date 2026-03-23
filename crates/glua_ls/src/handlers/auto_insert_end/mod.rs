mod auto_insert_end_request;

use std::str::FromStr;

use glua_code_analysis::{EmmyLuaAnalysis, FileId};
use glua_parser::{LuaAstNode, LuaClosureExpr, LuaDoStat, LuaForRangeStat, LuaForStat, LuaFuncStat, LuaIfClauseStat, LuaIfStat, LuaLocalFuncStat, LuaRepeatStat, LuaTokenKind, LuaWhileStat};
use lsp_types::Uri;
use rowan::TokenAtOffset;
use tokio_util::sync::CancellationToken;

use crate::{
    context::ServerContextSnapshot,
    handlers::auto_insert_end::auto_insert_end_request::{AutoInsertEndParams, AutoInsertEndResponse},
};

pub use auto_insert_end_request::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoInsertCloser {
    End,
    Until,
}

impl AutoInsertCloser {
    fn keyword(self) -> &'static str {
        match self {
            AutoInsertCloser::End => "end",
            AutoInsertCloser::Until => "until",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoInsertBlockKind {
    If,
    While,
    Do,
    For,
    Repeat,
    Function,
}

impl AutoInsertBlockKind {
    fn as_str(self) -> &'static str {
        match self {
            AutoInsertBlockKind::If => "if",
            AutoInsertBlockKind::While => "while",
            AutoInsertBlockKind::Do => "do",
            AutoInsertBlockKind::For => "for",
            AutoInsertBlockKind::Repeat => "repeat",
            AutoInsertBlockKind::Function => "function",
        }
    }
}

#[derive(Debug, Clone)]
enum AutoInsertCandidate {
    If(LuaIfStat),
    While(LuaWhileStat),
    Do(LuaDoStat),
    For(LuaForStat),
    ForRange(LuaForRangeStat),
    Repeat(LuaRepeatStat),
    Function(LuaFuncStat),
    LocalFunction(LuaLocalFuncStat),
    Closure(LuaClosureExpr),
}

impl AutoInsertCandidate {
    fn has_closer(&self) -> bool {
        match self {
            AutoInsertCandidate::If(candidate) => candidate.token_by_kind(LuaTokenKind::TkEnd).is_some(),
            AutoInsertCandidate::While(candidate) => candidate.token_by_kind(LuaTokenKind::TkEnd).is_some(),
            AutoInsertCandidate::Do(candidate) => candidate.token_by_kind(LuaTokenKind::TkEnd).is_some(),
            AutoInsertCandidate::For(candidate) => candidate.token_by_kind(LuaTokenKind::TkEnd).is_some(),
            AutoInsertCandidate::ForRange(candidate) => candidate.token_by_kind(LuaTokenKind::TkEnd).is_some(),
            AutoInsertCandidate::Repeat(candidate) => candidate.token_by_kind(LuaTokenKind::TkUntil).is_some(),
            AutoInsertCandidate::Function(candidate) => candidate.token_by_kind(LuaTokenKind::TkEnd).is_some(),
            AutoInsertCandidate::LocalFunction(candidate) => candidate.token_by_kind(LuaTokenKind::TkEnd).is_some(),
            AutoInsertCandidate::Closure(candidate) => candidate.token_by_kind(LuaTokenKind::TkEnd).is_some(),
        }
    }
}

pub async fn on_auto_insert_end_handler(
    context: ServerContextSnapshot,
    params: AutoInsertEndParams,
    cancel_token: CancellationToken,
) -> Option<AutoInsertEndResponse> {
    let uri = Uri::from_str(&params.uri).ok()?;
    if !context
        .wait_until_latest_document_version_applied(&uri, &cancel_token)
        .await
    {
        return Some(AutoInsertEndResponse {
            should_insert: false,
            close_keyword: String::new(),
            block_kind: None,
            reason: Some("stale-document-version".to_string()),
        });
    }

    let analysis = context.read_analysis(&cancel_token).await?;
    let file_id = analysis.get_file_id(&uri)?;
    let vfs = analysis.compilation.get_db().get_vfs();
    if vfs.get_file_version(&file_id) != Some(params.version) {
        return Some(AutoInsertEndResponse {
            should_insert: false,
            close_keyword: String::new(),
            block_kind: None,
            reason: Some("stale-document-version".to_string()),
        });
    }

    build_auto_insert_end_response(&analysis, file_id, params.position)
}

pub(crate) fn build_auto_insert_end_response(
    analysis: &EmmyLuaAnalysis,
    file_id: FileId,
    position: lsp_types::Position,
) -> Option<AutoInsertEndResponse> {
    let semantic_model = analysis.compilation.get_semantic_model(file_id)?;
    let document = semantic_model.get_document();
    let root = semantic_model.get_root();
    let offset = document.get_offset(position.line as usize, position.character as usize)?;
    if offset > root.syntax().text_range().end() {
        return Some(reject("cursor-outside-document"));
    }

    let line_range = document.get_line_range(position.line as usize)?;
    let line_text = document.get_text_slice(line_range);
    let cursor_in_line: usize = (offset - line_range.start()).into();
    let before_cursor = &line_text[..cursor_in_line];
    let after_cursor = &line_text[cursor_in_line..];

    if after_cursor.trim().is_empty() == false {
        return Some(reject("text-after-cursor"));
    }

    let trimmed = before_cursor.trim();
    let Some((block_kind, closer)) = match_auto_insert_block_kind(trimmed) else {
        return Some(reject("not-an-auto-insert-opener"));
    };

    let token = match root.syntax().token_at_offset(offset) {
        TokenAtOffset::Single(token) => token,
        TokenAtOffset::Between(left, _) => left,
        TokenAtOffset::None => return Some(reject("no-token-at-position")),
    };

    if is_trivia_or_literal_token(token.kind().into()) {
        return Some(reject("cursor-in-trivia"));
    }

    let candidate = resolve_candidate(token, block_kind)?;
    if candidate.has_closer() {
        return Some(reject("closing-token-already-present"));
    }

    Some(AutoInsertEndResponse {
        should_insert: true,
        close_keyword: closer.keyword().to_string(),
        block_kind: Some(block_kind.as_str().to_string()),
        reason: None,
    })
}

fn resolve_candidate(token: glua_parser::LuaSyntaxToken, block_kind: AutoInsertBlockKind) -> Option<AutoInsertCandidate> {
    token.parent_ancestors().find_map(|node| match block_kind {
        AutoInsertBlockKind::If => LuaIfStat::cast(node.clone())
            .map(AutoInsertCandidate::If)
            .or_else(|| {
                LuaIfClauseStat::cast(node.clone())
                    .and_then(|clause| clause.get_parent_if_stat().map(AutoInsertCandidate::If))
            }),
        AutoInsertBlockKind::While => LuaWhileStat::cast(node.clone()).map(AutoInsertCandidate::While),
        AutoInsertBlockKind::Do => LuaDoStat::cast(node.clone()).map(AutoInsertCandidate::Do),
        AutoInsertBlockKind::For => LuaForStat::cast(node.clone())
            .map(AutoInsertCandidate::For)
            .or_else(|| LuaForRangeStat::cast(node.clone()).map(AutoInsertCandidate::ForRange)),
        AutoInsertBlockKind::Repeat => LuaRepeatStat::cast(node.clone()).map(AutoInsertCandidate::Repeat),
        AutoInsertBlockKind::Function => LuaFuncStat::cast(node.clone())
            .map(AutoInsertCandidate::Function)
            .or_else(|| LuaClosureExpr::cast(node.clone()).map(AutoInsertCandidate::Closure))
            .or_else(|| LuaLocalFuncStat::cast(node.clone()).map(AutoInsertCandidate::LocalFunction)),
    })
}

fn match_auto_insert_block_kind(text: &str) -> Option<(AutoInsertBlockKind, AutoInsertCloser)> {
    if text == "repeat" {
        return Some((AutoInsertBlockKind::Repeat, AutoInsertCloser::Until));
    }

    if text == "else" {
        return Some((AutoInsertBlockKind::If, AutoInsertCloser::End));
    }

    if starts_with_keyword(text, "elseif") && ends_with_keyword(text, "then") {
        return Some((AutoInsertBlockKind::If, AutoInsertCloser::End));
    }

    if starts_with_keyword(text, "if") && ends_with_keyword(text, "then") {
        return Some((AutoInsertBlockKind::If, AutoInsertCloser::End));
    }

    if starts_with_keyword(text, "while") && ends_with_keyword(text, "do") {
        return Some((AutoInsertBlockKind::While, AutoInsertCloser::End));
    }

    if starts_with_keyword(text, "for") && ends_with_keyword(text, "do") {
        return Some((AutoInsertBlockKind::For, AutoInsertCloser::End));
    }

    if text == "do" {
        return Some((AutoInsertBlockKind::Do, AutoInsertCloser::End));
    }

    if is_function_header(text) {
        return Some((AutoInsertBlockKind::Function, AutoInsertCloser::End));
    }

    None
}

fn is_function_header(text: &str) -> bool {
    (starts_with_keyword(text, "function")
        || starts_with_keyword(text, "local function")
        || contains_assigned_function(text))
        && text.ends_with(')')
}

fn starts_with_keyword(text: &str, keyword: &str) -> bool {
    if text == keyword {
        return true;
    }

    text.strip_prefix(keyword)
        .is_some_and(|rest| rest.chars().next().is_some_and(|ch| ch.is_whitespace() || ch == '('))
}

fn ends_with_keyword(text: &str, keyword: &str) -> bool {
    text.split_whitespace().last() == Some(keyword)
}

fn contains_assigned_function(text: &str) -> bool {
    let Some(function_index) = text.find("function") else {
        return false;
    };

    text[..function_index].trim_end().ends_with('=')
}

fn is_trivia_or_literal_token(kind: LuaTokenKind) -> bool {
    matches!(
        kind,
        LuaTokenKind::TkShortComment
            | LuaTokenKind::TkLongComment
            | LuaTokenKind::TKNonStdComment
            | LuaTokenKind::TkString
            | LuaTokenKind::TkLongString
            | LuaTokenKind::TkNormalStart
            | LuaTokenKind::TkLongCommentStart
            | LuaTokenKind::TkDocLongStart
            | LuaTokenKind::TkDocStart
            | LuaTokenKind::TKDocTriviaStart
            | LuaTokenKind::TkDocTrivia
            | LuaTokenKind::TkDocContinue
            | LuaTokenKind::TkDocContinueOr
        )
}

fn reject(reason: &'static str) -> AutoInsertEndResponse {
    AutoInsertEndResponse {
        should_insert: false,
        close_keyword: String::new(),
        block_kind: None,
        reason: Some(reason.to_string()),
    }
}
