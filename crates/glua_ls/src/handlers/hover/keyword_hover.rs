use crate::meta_text::meta_keyword;
use glua_parser::{LuaSyntaxToken, LuaTokenKind};

pub fn is_keyword(token: LuaSyntaxToken) -> bool {
    matches!(
        token.kind().into(),
        LuaTokenKind::TkLocal
            | LuaTokenKind::TkFunction
            | LuaTokenKind::TkEnd
            | LuaTokenKind::TkIf
            | LuaTokenKind::TkThen
            | LuaTokenKind::TkElse
            | LuaTokenKind::TkElseIf
            | LuaTokenKind::TkWhile
            | LuaTokenKind::TkDo
            | LuaTokenKind::TkFor
            | LuaTokenKind::TkIn
            | LuaTokenKind::TkRepeat
            | LuaTokenKind::TkUntil
            | LuaTokenKind::TkReturn
            | LuaTokenKind::TkBreak
            | LuaTokenKind::TkGoto
    )
}

// todo add usage
pub fn hover_keyword(token: LuaSyntaxToken) -> String {
    match token.kind().into() {
        LuaTokenKind::TkLocal => meta_keyword("local"),
        LuaTokenKind::TkFunction => meta_keyword("function"),
        LuaTokenKind::TkEnd => meta_keyword("end"),
        LuaTokenKind::TkIf => meta_keyword("if"),
        LuaTokenKind::TkThen => meta_keyword("then"),
        LuaTokenKind::TkElse => meta_keyword("else"),
        LuaTokenKind::TkElseIf => meta_keyword("elseif"),
        LuaTokenKind::TkWhile => meta_keyword("while"),
        LuaTokenKind::TkDo => meta_keyword("do"),
        LuaTokenKind::TkFor => meta_keyword("for"),
        LuaTokenKind::TkIn => meta_keyword("in"),
        LuaTokenKind::TkRepeat => meta_keyword("repeat"),
        LuaTokenKind::TkUntil => meta_keyword("until"),
        LuaTokenKind::TkReturn => meta_keyword("return"),
        LuaTokenKind::TkBreak => meta_keyword("break"),
        LuaTokenKind::TkGoto => meta_keyword("goto"),
        _ => "".to_string(),
    }
}
