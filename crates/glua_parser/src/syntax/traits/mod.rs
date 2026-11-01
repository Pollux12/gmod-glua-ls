mod comment_trait;
mod description_trait;

use std::marker::PhantomData;

use rowan::{TextRange, TextSize, WalkEvent};

use crate::{
    LuaAstPtr,
    kind::{LuaSyntaxKind, LuaTokenKind},
};

use super::LuaSyntaxId;
pub use super::{
    LuaSyntaxElementChildren, LuaSyntaxNode, LuaSyntaxNodeChildren, LuaSyntaxToken, node::*,
};
pub use comment_trait::*;
pub use description_trait::*;

/// Typed walks from a syntax node.
pub trait LuaSyntaxNodeExt {
    /// The descendants of type `N`, this node included, in preorder. Only
    /// they and their ancestors get syntax nodes of their own.
    fn descendants_of<N: LuaAstNode>(&self) -> impl Iterator<Item = N>;
}

impl LuaSyntaxNodeExt for LuaSyntaxNode {
    fn descendants_of<N: LuaAstNode>(&self) -> impl Iterator<Item = N> {
        self.descendants_matching(|kind| N::can_cast(kind.into()))
            .filter_map(N::cast)
    }
}

pub trait LuaAstNode {
    fn syntax(&self) -> &LuaSyntaxNode;

    fn can_cast(kind: LuaSyntaxKind) -> bool
    where
        Self: Sized;

    fn cast(syntax: LuaSyntaxNode) -> Option<Self>
    where
        Self: Sized;

    fn child<N: LuaAstNode>(&self) -> Option<N> {
        self.syntax().children().find_map(N::cast)
    }

    fn token<N: LuaAstToken>(&self) -> Option<N> {
        self.syntax()
            .children_with_tokens()
            .find_map(|it| it.into_token().and_then(N::cast))
    }

    fn token_by_kind(&self, kind: LuaTokenKind) -> Option<LuaGeneralToken> {
        let token = self
            .syntax()
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == kind.into())?;

        LuaGeneralToken::cast(token)
    }

    fn tokens<N: LuaAstToken>(&self) -> LuaAstTokenChildren<N> {
        LuaAstTokenChildren::new(self.syntax())
    }

    fn children<N: LuaAstNode>(&self) -> LuaAstChildren<N> {
        LuaAstChildren::new(self.syntax())
    }

    fn descendants<N: LuaAstNode>(&self) -> impl Iterator<Item = N> {
        self.syntax().descendants_of()
    }

    fn walk_descendants<N: LuaAstNode>(&self) -> impl Iterator<Item = WalkEvent<N>> {
        self.syntax().preorder().filter_map(|event| match event {
            WalkEvent::Enter(node) => N::cast(node).map(WalkEvent::Enter),
            WalkEvent::Leave(node) => N::cast(node).map(WalkEvent::Leave),
        })
    }

    fn ancestors<N: LuaAstNode>(&self) -> impl Iterator<Item = N> {
        self.syntax().ancestors().filter_map(N::cast)
    }

    fn get_root(&self) -> LuaSyntaxNode {
        let syntax = self.syntax();
        if syntax.kind() == LuaSyntaxKind::Chunk.into() {
            syntax.clone()
        } else {
            syntax.ancestors().last().unwrap()
        }
    }

    fn get_parent<N: LuaAstNode>(&self) -> Option<N> {
        self.syntax().parent().and_then(N::cast)
    }

    fn get_position(&self) -> TextSize {
        let range = self.syntax().text_range();
        range.start()
    }

    fn get_range(&self) -> TextRange {
        self.syntax().text_range()
    }

    fn get_syntax_id(&self) -> LuaSyntaxId {
        LuaSyntaxId::from_node(self.syntax())
    }

    fn get_text(&self) -> String {
        format!("{}", self.syntax().text())
    }

    fn dump(&self) -> String {
        format!("{:#?}", self.syntax())
    }

    fn to_ptr(&self) -> LuaAstPtr<Self>
    where
        Self: Sized,
    {
        LuaAstPtr::new(self)
    }
}

/// An iterator over `SyntaxNode` children of a particular AST type.
#[derive(Debug, Clone)]
pub struct LuaAstChildren<N> {
    inner: LuaSyntaxNodeChildren,
    ph: PhantomData<N>,
}

impl<N> LuaAstChildren<N> {
    pub fn new(parent: &LuaSyntaxNode) -> LuaAstChildren<N> {
        LuaAstChildren {
            inner: parent.children(),
            ph: PhantomData,
        }
    }
}

impl<N: LuaAstNode> Iterator for LuaAstChildren<N> {
    type Item = N;

    fn next(&mut self) -> Option<N> {
        self.inner.find_map(N::cast)
    }
}

pub trait LuaAstToken {
    fn syntax(&self) -> &LuaSyntaxToken;

    fn can_cast(kind: LuaTokenKind) -> bool
    where
        Self: Sized;

    fn cast(syntax: LuaSyntaxToken) -> Option<Self>
    where
        Self: Sized;

    fn get_token_kind(&self) -> LuaTokenKind {
        self.syntax().kind().into()
    }

    fn get_position(&self) -> TextSize {
        let range = self.syntax().text_range();
        range.start()
    }

    fn get_range(&self) -> TextRange {
        self.syntax().text_range()
    }

    fn get_syntax_id(&self) -> LuaSyntaxId {
        LuaSyntaxId::from_token(self.syntax())
    }

    fn get_text(&self) -> &str {
        self.syntax().text()
    }

    fn slice(&self, range: TextRange) -> Option<&str> {
        let text = self.get_text();
        let self_range = self.get_range();
        if range.start() >= self_range.start() && range.end() <= self_range.end() {
            let start = (range.start() - self_range.start()).into();
            let end = (range.end() - self_range.start()).into();
            text.get(start..end)
        } else {
            None
        }
    }

    fn get_parent<N: LuaAstNode>(&self) -> Option<N> {
        self.syntax().parent().and_then(N::cast)
    }

    fn ancestors<N: LuaAstNode>(&self) -> impl Iterator<Item = N> {
        self.syntax().parent_ancestors().filter_map(N::cast)
    }

    fn dump(&self) -> String {
        format!("{:#?}", self.syntax())
    }
}

#[derive(Debug, Clone)]
pub struct LuaAstTokenChildren<N> {
    inner: LuaSyntaxElementChildren,
    ph: PhantomData<N>,
}

impl<N> LuaAstTokenChildren<N> {
    pub fn new(parent: &LuaSyntaxNode) -> LuaAstTokenChildren<N> {
        LuaAstTokenChildren {
            inner: parent.children_with_tokens(),
            ph: PhantomData,
        }
    }
}

impl<N: LuaAstToken> Iterator for LuaAstTokenChildren<N> {
    type Item = N;

    fn next(&mut self) -> Option<N> {
        self.inner.find_map(|it| it.into_token().and_then(N::cast))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LuaParser, ParserConfig};

    const SAMPLE: &str = r#"
---@class Shop
---@field price integer
local Shop = {}

---@param count integer
---@return integer
function Shop:Buy(count)
    local total = self.price * count
    for i = 1, count do
        if i % 2 == 0 then
            total = total + helper(i, { a = 1, [i] = function() return i end })
        elseif i > 3 then
            break
        else
            repeat total = total - 1 until total < 0
        end
    end
    for _, v in pairs(self.items or {}) do
        while v.next do v = v.next end
    end
    return setmetatable({ total = total }, { __index = Shop })
end

local function helper(a, b) return a + #b end
Config = Config or {}
Config.Money = helper(1, "text") .. tostring(nil)
"#;

    /// The nodes of type `N` under `node`, as the plain preorder walk and the
    /// green-guided walk each find them.
    fn both<N: LuaAstNode>(node: &LuaSyntaxNode) -> (Vec<LuaSyntaxNode>, Vec<LuaSyntaxNode>) {
        let plain = node
            .descendants()
            .filter_map(N::cast)
            .map(|it| it.syntax().clone())
            .collect();
        let guided = node
            .descendants_matching(|kind| N::can_cast(kind.into()))
            .filter_map(N::cast)
            .map(|it| it.syntax().clone())
            .collect();
        (plain, guided)
    }

    fn assert_same<N: LuaAstNode>(node: &LuaSyntaxNode) {
        let (plain, guided) = both::<N>(node);
        assert!(
            !plain.is_empty(),
            "the sample has a {}",
            std::any::type_name::<N>()
        );
        assert_eq!(guided, plain, "{}", std::any::type_name::<N>());
        for (guided, plain) in guided.iter().zip(&plain) {
            assert_eq!(
                (guided.parent(), guided.index(), guided.text_range()),
                (plain.parent(), plain.index(), plain.text_range())
            );
        }
    }

    #[test]
    fn the_green_guided_walk_finds_what_the_preorder_walk_finds() {
        let root = LuaParser::parse(SAMPLE, ParserConfig::default()).get_red_root();
        assert_same::<LuaAst>(&root);
        assert_same::<LuaStat>(&root);
        assert_same::<LuaExpr>(&root);
        assert_same::<LuaCallExpr>(&root);
        assert_same::<LuaIndexExpr>(&root);
        assert_same::<LuaNameExpr>(&root);
        assert_same::<LuaTableExpr>(&root);
        assert_same::<LuaClosureExpr>(&root);
        assert_same::<LuaFuncStat>(&root);
        assert_same::<LuaLoopStat>(&root);
        assert_same::<LuaVarExpr>(&root);
        assert_same::<LuaDocTag>(&root);
        assert_same::<LuaDocType>(&root);
        assert_same::<LuaComment>(&root);
    }

    #[test]
    fn the_green_guided_walk_starts_at_an_inner_node() {
        let root = LuaParser::parse(SAMPLE, ParserConfig::default()).get_red_root();
        let func_stat = root
            .descendants()
            .find_map(LuaFuncStat::cast)
            .expect("the sample has a method");
        assert_same::<LuaFuncStat>(func_stat.syntax());
        assert_same::<LuaCallExpr>(func_stat.syntax());
        assert_same::<LuaExpr>(func_stat.syntax());
    }
}
