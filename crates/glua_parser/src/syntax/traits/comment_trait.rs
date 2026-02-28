use crate::{
    LuaKind,
    kind::{LuaSyntaxKind, LuaTokenKind},
};

use super::{LuaAstNode, LuaComment, LuaSyntaxNode};

#[allow(unused)]
pub trait LuaCommentOwner: LuaAstNode {
    fn get_comments(&self) -> Vec<LuaComment> {
        let mut comments = vec![];
        if let Some(attached_comment) = find_attached_comment(self.syntax()) {
            comments.push(LuaComment::cast(attached_comment).unwrap());
        }

        if let Some(inline_comment) = find_inline_comment(self.syntax()) {
            comments.push(LuaComment::cast(inline_comment).unwrap());
        }

        comments
    }

    fn get_left_comment(&self) -> Option<LuaComment> {
        if let Some(attached_comment) = find_attached_comment(self.syntax()) {
            return LuaComment::cast(attached_comment);
        }

        None
    }
}

fn find_attached_comment(node: &LuaSyntaxNode) -> Option<LuaSyntaxNode> {
    let mut prev_sibling = node.prev_sibling_or_token();
    let mut meet_end_of_line = false;
    for _ in 0..=2 {
        prev_sibling.as_ref()?;

        if let Some(sibling) = &prev_sibling {
            match sibling.kind() {
                LuaKind::Token(LuaTokenKind::TkWhitespace) => {}
                LuaKind::Token(LuaTokenKind::TkEndOfLine) => {
                    if meet_end_of_line {
                        return None;
                    }
                    meet_end_of_line = true;
                }
                LuaKind::Syntax(LuaSyntaxKind::Comment) => return sibling.clone().into_node(),
                _ => {
                    return None;
                }
            }
        }
        prev_sibling = prev_sibling.unwrap().prev_sibling_or_token();
    }

    None
}

fn find_inline_comment(node: &LuaSyntaxNode) -> Option<LuaSyntaxNode> {
    let mut next_sibling = node.next_sibling_or_token();
    for _ in 0..=3 {
        next_sibling.as_ref()?;

        if let Some(sibling) = &next_sibling {
            match sibling.kind() {
                LuaKind::Token(
                    LuaTokenKind::TkWhitespace | LuaTokenKind::TkComma | LuaTokenKind::TkSemicolon,
                ) => {}
                LuaKind::Syntax(LuaSyntaxKind::Comment) => return sibling.clone().into_node(),
                _ => {
                    return None;
                }
            }
        }
        next_sibling = next_sibling.unwrap().next_sibling_or_token();
    }

    None
}
