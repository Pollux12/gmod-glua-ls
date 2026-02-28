use rowan::{GreenNode, NodeCache};

use crate::{
    kind::{LuaSyntaxKind, LuaTokenKind},
    text::SourceRange,
};

#[derive(Debug, Clone)]
enum LuaGreenElement {
    None,
    Node {
        kind: LuaSyntaxKind,
        children: Vec<usize>,
    },
    Token {
        kind: LuaTokenKind,
        range: SourceRange,
    },
}
/// A builder for a green tree.
#[derive(Default, Debug)]
pub struct LuaGreenNodeBuilder<'cache> {
    parents: Vec<(LuaSyntaxKind, usize)>,
    children: Vec<usize>, /*index for elements*/
    elements: Vec<LuaGreenElement>,
    builder: rowan::GreenNodeBuilder<'cache>,
}

impl LuaGreenNodeBuilder<'_> {
    /// Creates new builder.
    pub fn new() -> LuaGreenNodeBuilder<'static> {
        LuaGreenNodeBuilder::default()
    }

    pub fn with_cache(cache: &mut NodeCache) -> LuaGreenNodeBuilder<'_> {
        LuaGreenNodeBuilder {
            parents: Vec::new(),
            children: Vec::new(),
            elements: Vec::new(),
            builder: rowan::GreenNodeBuilder::with_cache(cache),
        }
    }

    #[inline]
    pub fn token(&mut self, kind: LuaTokenKind, range: SourceRange) {
        let len = self.elements.len();
        self.elements.push(LuaGreenElement::Token { kind, range });
        self.children.push(len);
    }

    #[inline]
    pub fn start_node(&mut self, kind: LuaSyntaxKind) {
        let len = self.children.len();
        self.parents.push((kind, len));
    }

    #[inline]
    pub fn finish_node(&mut self) {
        if self.parents.is_empty() || self.children.is_empty() {
            return;
        }

        let (parent_kind, mut first_start) = self.parents.pop().unwrap();
        let mut child_start = first_start;
        let mut child_end = self.children.len() - 1;
        let child_count = self.children.len();
        let green = match parent_kind {
            LuaSyntaxKind::Block | LuaSyntaxKind::Chunk => {
                while child_start > 0 {
                    if self.is_trivia(self.children[child_start - 1]) {
                        child_start -= 1;
                    } else {
                        break;
                    }
                }
                if child_start < first_start {
                    first_start = child_start;
                }

                let children = self.children.drain(first_start..).collect::<Vec<_>>();

                LuaGreenElement::Node {
                    kind: parent_kind,
                    children,
                }
            }
            LuaSyntaxKind::Comment | LuaSyntaxKind::TypeMultiLineUnion => {
                while child_start < child_count {
                    if self.is_trivia_whitespace(self.children[child_start]) {
                        child_start += 1;
                    } else {
                        break;
                    }
                }
                while child_end > child_start {
                    if self.is_trivia_whitespace(self.children[child_end]) {
                        child_end -= 1;
                    } else {
                        break;
                    }
                }

                let children = self
                    .children
                    .drain(child_start..=child_end)
                    .collect::<Vec<_>>();
                LuaGreenElement::Node {
                    kind: parent_kind,
                    children,
                }
            }
            _ => {
                while child_start < child_count {
                    if self.is_trivia(self.children[child_start]) {
                        child_start += 1;
                    } else {
                        break;
                    }
                }
                while child_end > child_start {
                    if self.is_trivia(self.children[child_end]) {
                        child_end -= 1;
                    } else {
                        break;
                    }
                }

                let children = self
                    .children
                    .drain(child_start..=child_end)
                    .collect::<Vec<_>>();
                LuaGreenElement::Node {
                    kind: parent_kind,
                    children,
                }
            }
        };

        let pos = self.elements.len();
        self.elements.push(green);

        if child_end + 1 < child_count {
            self.children.insert(child_start, pos);
        } else {
            self.children.push(pos);
        }
    }

    fn is_trivia(&self, pos: usize) -> bool {
        self.elements.get(pos).is_some_and(|element| {
            matches!(
                element,
                LuaGreenElement::Token {
                    kind: LuaTokenKind::TkWhitespace
                        | LuaTokenKind::TkEndOfLine
                        | LuaTokenKind::TkDocContinue,
                    ..
                } | LuaGreenElement::Node {
                    kind: LuaSyntaxKind::Comment | LuaSyntaxKind::DocDescription,
                    ..
                }
            )
        })
    }

    pub fn is_trivia_whitespace(&self, pos: usize) -> bool {
        if let Some(element) = self.elements.get(pos) {
            matches!(
                element,
                LuaGreenElement::Token {
                    kind: LuaTokenKind::TkWhitespace | LuaTokenKind::TkEndOfLine,
                    ..
                }
            )
        } else {
            false
        }
    }

    fn build_rowan_green(&mut self, parent: usize, text: &str) {
        struct StackItem {
            index: usize,
            is_close: bool,
        }

        let mut stack = vec![StackItem {
            index: parent,
            is_close: false,
        }];

        while let Some(item) = stack.pop() {
            if item.is_close {
                self.builder.finish_node();
                continue;
            }

            let element = std::mem::replace(&mut self.elements[item.index], LuaGreenElement::None);
            match element {
                LuaGreenElement::Node { kind, children } => {
                    self.builder.start_node(kind.into());
                    stack.push(StackItem {
                        index: item.index,
                        is_close: true,
                    });

                    for child in children.iter().rev() {
                        stack.push(StackItem {
                            index: *child,
                            is_close: false,
                        });
                    }
                }
                LuaGreenElement::Token { kind, range } => {
                    let start = range.start_offset;
                    let end = range.end_offset();
                    let token_text = &text[start..end];
                    self.builder.token(kind.into(), token_text);
                }
                _ => {}
            }
        }
    }

    #[inline]
    pub fn finish(mut self, text: &str) -> GreenNode {
        if let Some(root_pos) = self.children.first() {
            let is_chunk_root = matches!(
                self.elements[*root_pos],
                LuaGreenElement::Node {
                    kind: LuaSyntaxKind::Chunk,
                    ..
                }
            );
            if !is_chunk_root {
                self.builder.start_node(LuaSyntaxKind::Chunk.into());
            }

            self.build_rowan_green(*root_pos, text);

            if !is_chunk_root {
                self.builder.finish_node();
            }

            return self.builder.finish();
        }

        self.builder.start_node(LuaSyntaxKind::Chunk.into());
        self.builder.finish_node();
        self.builder.finish()
    }
}
