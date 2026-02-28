use crate::{LuaAstNode, LuaDocDescription, LuaKind, LuaSyntaxKind, LuaSyntaxNode, LuaTokenKind};

#[allow(unused)]
pub trait LuaDocDescriptionOwner: LuaAstNode {
    fn get_description(&self) -> Option<LuaDocDescription> {
        if let Some(inline_description) = find_inline_description(self.syntax()) {
            return LuaDocDescription::cast(inline_description);
        }

        None
    }

    fn get_descriptions(&self) -> Vec<LuaDocDescription> {
        let mut descriptions = vec![];
        if let Some(attached_description) = find_attached_description(self.syntax()) {
            descriptions.push(LuaDocDescription::cast(attached_description).unwrap());
        }

        if let Some(inline_description) = find_inline_description(self.syntax()) {
            descriptions.push(LuaDocDescription::cast(inline_description).unwrap());
        }

        descriptions
    }
}

fn find_attached_description(node: &LuaSyntaxNode) -> Option<LuaSyntaxNode> {
    let mut prev_sibling = node.prev_sibling_or_token();
    let mut meet_end_of_line = false;
    for _ in 0..=5 {
        prev_sibling.as_ref()?;

        if let Some(sibling) = &prev_sibling {
            match sibling.kind() {
                LuaKind::Token(
                    LuaTokenKind::TkWhitespace
                    | LuaTokenKind::TkDocContinue
                    | LuaTokenKind::TkDocStart,
                ) => {}
                LuaKind::Token(LuaTokenKind::TkEndOfLine) => {
                    if meet_end_of_line {
                        return None;
                    }
                    meet_end_of_line = true;
                }
                LuaKind::Syntax(LuaSyntaxKind::DocDescription) => {
                    let description_node = sibling.clone().into_node()?;
                    if !check_is_inline_description(&description_node).unwrap_or(false) {
                        return Some(description_node);
                    }
                    return None;
                }
                _ => {
                    return None;
                }
            }
        }
        prev_sibling = prev_sibling.unwrap().prev_sibling_or_token();
    }

    None
}

fn check_is_inline_description(node: &LuaSyntaxNode) -> Option<bool> {
    let mut prev_sibling = node.prev_sibling_or_token();
    for _ in 0..=3 {
        prev_sibling.as_ref()?;

        if let Some(sibling) = &prev_sibling {
            match sibling.kind() {
                LuaKind::Token(LuaTokenKind::TkWhitespace | LuaTokenKind::TkDocContinue) => {}
                LuaKind::Token(LuaTokenKind::TkEndOfLine | LuaTokenKind::TkNormalStart) => {
                    return Some(false);
                }
                _ => {
                    return Some(true);
                }
            }
        }
        prev_sibling = prev_sibling.unwrap().prev_sibling_or_token();
    }

    Some(false)
}

fn find_inline_description(node: &LuaSyntaxNode) -> Option<LuaSyntaxNode> {
    let mut next_sibling = node.next_sibling_or_token();
    for _ in 0..=3 {
        next_sibling.as_ref()?;

        if let Some(sibling) = &next_sibling {
            match sibling.kind() {
                LuaKind::Token(LuaTokenKind::TkWhitespace) => {}
                LuaKind::Syntax(LuaSyntaxKind::DocDescription) => {
                    return sibling.clone().into_node();
                }
                _ => {
                    return None;
                }
            }
        }
        next_sibling = next_sibling.unwrap().next_sibling_or_token();
    }

    None
}
