mod node;
mod traits;
mod tree;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::iter::successors;
use std::marker::PhantomData;

use rowan::{Language, TextRange, TextSize};

use crate::kind::{LuaKind, LuaSyntaxKind, LuaTokenKind};
pub use traits::*;
pub use tree::{LuaSyntaxTree, LuaTreeBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LuaLanguage;

impl Language for LuaLanguage {
    type Kind = LuaKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        LuaKind::from_raw(raw.0)
    }

    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind.get_raw())
    }
}

pub type LuaSyntaxNode = rowan::SyntaxNode<LuaLanguage>;
pub type LuaSyntaxToken = rowan::SyntaxToken<LuaLanguage>;
pub type LuaSyntaxElement = rowan::NodeOrToken<LuaSyntaxNode, LuaSyntaxToken>;
pub type LuaSyntaxElementChildren = rowan::SyntaxElementChildren<LuaLanguage>;
pub type LuaSyntaxNodeChildren = rowan::SyntaxNodeChildren<LuaLanguage>;
pub type LuaSyntaxNodePtr = rowan::ast::SyntaxNodePtr<LuaLanguage>;

impl From<LuaSyntaxKind> for rowan::SyntaxKind {
    fn from(kind: LuaSyntaxKind) -> Self {
        let lua_kind = LuaKind::from(kind);
        rowan::SyntaxKind(lua_kind.get_raw())
    }
}

impl From<rowan::SyntaxKind> for LuaSyntaxKind {
    fn from(kind: rowan::SyntaxKind) -> Self {
        LuaKind::from_raw(kind.0).into()
    }
}

impl From<LuaTokenKind> for rowan::SyntaxKind {
    fn from(kind: LuaTokenKind) -> Self {
        let lua_kind = LuaKind::from(kind);
        rowan::SyntaxKind(lua_kind.get_raw())
    }
}

impl From<rowan::SyntaxKind> for LuaTokenKind {
    fn from(kind: rowan::SyntaxKind) -> Self {
        LuaKind::from_raw(kind.0).into()
    }
}

/// A resolver for one red syntax root. Keeping the root alongside its
/// nodes makes identity comparison sound for the resolver's lifetime.
#[derive(Debug)]
pub struct LuaSyntaxResolver {
    pub(crate) root: LuaSyntaxNode,
    pub(crate) entries: rustc_hash::FxHashMap<LuaSyntaxId, Option<LuaSyntaxNode>>,
}

impl LuaSyntaxResolver {
    /// Creates a resolver whose cache is released when the resolver is dropped.
    pub fn new(root: LuaSyntaxNode) -> Self {
        Self {
            root,
            entries: rustc_hash::FxHashMap::default(),
        }
    }

    /// Resolves an id against this resolver's root, retaining the result.
    pub fn resolve(&mut self, id: LuaSyntaxId) -> Option<LuaSyntaxNode> {
        if let Some(hit) = self.entries.get(&id) {
            return hit.clone();
        }

        let resolved = if id.range.is_empty() {
            id.walk_from_root(&self.root)
        } else {
            self.walk(id)
        };
        self.entries.insert(id, resolved.clone());
        resolved
    }

    /// [`LuaSyntaxId::walk_from_root`], taking each node on the way down that
    /// this resolver already holds and keeping each one it creates: nodes
    /// resolved under one parent share it rather than each creating a copy.
    fn walk(&mut self, id: LuaSyntaxId) -> Option<LuaSyntaxNode> {
        let mut node = self.root.clone();
        loop {
            if node.text_range() == id.range && node.kind() == id.kind {
                return Some(node);
            }
            let (kind, range, is_node) = node.child_at_range_info(id.range)?;
            if !is_node {
                return None;
            }
            let child_id = LuaSyntaxId { kind, range };
            node = match self.entries.get(&child_id) {
                Some(Some(child)) => child.clone(),
                _ => {
                    let child = node.child_or_token_at_range(id.range)?.into_node()?;
                    self.entries.insert(child_id, Some(child.clone()));
                    child
                }
            };
        }
    }
}

/// Per-thread memo for [`LuaSyntaxId::to_node_from_root`], keyed by root
/// (MRU, bounded number of roots kept). Holding each root alive keeps its
/// green tree alive, which is what makes identity comparison sound.
///
/// This is the only resolution store: it serves both the ad-hoc lookups and
/// the long analysis passes that used to keep a per-scope cache. A per-scope
/// cache rebuilt and then dropped every resolved id at each boundary — on a
/// workspace pass that meant re-walking the same nodes every file and every
/// wave — while a plain unbounded cache would retain every tree's resolved
/// set for the thread's life. The MRU bound keeps the working set (the roots
/// a pass is actually cycling) resident and lets colder roots rebuild their
/// handful of re-requested entries instead.
mod node_memo {
    use super::{LuaSyntaxId, LuaSyntaxNode, LuaSyntaxResolver};

    const MAX_ROOTS: usize = 32;

    #[derive(Default)]
    pub(super) struct NodeMemo {
        pub(super) roots: Vec<LuaSyntaxResolver>,
    }

    impl NodeMemo {
        pub(super) fn resolve(
            &mut self,
            id: LuaSyntaxId,
            root: &LuaSyntaxNode,
        ) -> Option<LuaSyntaxNode> {
            self.touch(root);
            self.roots[0].resolve(id)
        }

        /// Marks `root` most-recently-used, installing its resolver on miss.
        /// Returns after guaranteeing `roots[0]` resolves against `root`.
        pub(super) fn touch(&mut self, root: &LuaSyntaxNode) {
            let found = self.roots.iter().position(|cached| cached.root == *root);
            match found {
                Some(0) => {}
                Some(index) => self.roots.swap(0, index),
                None => {
                    if self.roots.len() == MAX_ROOTS {
                        self.roots.pop();
                    }
                    self.roots.insert(0, LuaSyntaxResolver::new(root.clone()));
                }
            }
        }
    }
}

thread_local! {
    static NODE_MEMO: std::cell::RefCell<node_memo::NodeMemo> =
        std::cell::RefCell::new(node_memo::NodeMemo::default());
}

/// Runs `f` with lazily created root resolvers on the current thread.
///
/// Resolution is backed by the thread's bounded MRU memo, so resolved nodes
/// stay warm across files and passes rather than being rebuilt and dropped at
/// each scope boundary.
pub fn with_syntax_resolvers<R>(f: impl FnOnce() -> R) -> R {
    f()
}

/// Runs `f` with `root` marked hot in the current thread's resolver memo.
///
/// Resolution is shared through the same bounded memo as
/// [`with_syntax_resolvers`]; marking the root only protects it from eviction
/// while the callback runs.
pub fn with_syntax_resolver<R>(root: &LuaSyntaxNode, f: impl FnOnce() -> R) -> R {
    NODE_MEMO.with(|memo| memo.borrow_mut().touch(root));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LuaParser, ParserConfig};

    fn parsed_root(source: &str) -> LuaSyntaxNode {
        LuaParser::parse(source, ParserConfig::default()).get_red_root()
    }

    fn memo_roots_len() -> usize {
        NODE_MEMO.with(|memo| memo.borrow().roots.len())
    }

    fn memo_has_root(root: &LuaSyntaxNode) -> bool {
        NODE_MEMO.with(|memo| memo.borrow().roots.iter().any(|r| r.root == *root))
    }

    fn memo_entries(root: &LuaSyntaxNode) -> Option<usize> {
        NODE_MEMO.with(|memo| {
            memo.borrow()
                .roots
                .iter()
                .find(|r| r.root == *root)
                .map(|r| r.entries.len())
        })
    }

    #[test]
    fn resolver_memo_caches_resolved_nodes() {
        let root = parsed_root("local value = 1\n");
        let id = LuaSyntaxId::from_node(&root.descendants().next().unwrap());

        with_syntax_resolver(&root, || {
            assert!(id.to_node_from_root(&root).is_some());
            assert_eq!(memo_entries(&root), Some(1));
        });
    }

    /// Every node resolves, from a fresh resolver and from one that already
    /// holds its ancestors, to the node a walk from the root finds.
    #[test]
    fn a_resolver_reusing_resolved_ancestors_finds_what_a_walk_from_the_root_finds() {
        let root = parsed_root(
            "local shop = { items = { 1, 2 } }
function shop:Buy(count)
    if count > 0 then
        return self.items[count] + helper(count, { a = 1 })
    end
end
",
        );
        let ids = root
            .descendants()
            .map(|node| LuaSyntaxId::from_node(&node))
            .collect::<Vec<_>>();
        let mut resolver = LuaSyntaxResolver::new(root.clone());

        for pass in 0..2 {
            for id in &ids {
                let expected = id.walk_from_root(&root);
                assert_eq!(resolver.resolve(*id), expected, "pass {pass}, {id:?}");
                assert_eq!(
                    resolver.resolve(*id).and_then(|node| node.parent()),
                    expected.and_then(|node| node.parent()),
                    "pass {pass}, parent of {id:?}"
                );
            }
        }
    }

    #[test]
    fn resolver_memo_supports_nested_roots_and_exact_matching() {
        let outer_root = parsed_root("local outer = 1\n");
        let inner_root = parsed_root("-- shift\nlocal inner = 2\n");
        let equivalent_root = parsed_root("local outer = 1\n");
        let outer_id = LuaSyntaxId::from_node(&outer_root.descendants().next().unwrap());
        let inner_id = LuaSyntaxId::from_node(&inner_root.descendants().next().unwrap());

        with_syntax_resolver(&outer_root, || {
            assert!(outer_id.to_node_from_root(&outer_root).is_some());
            with_syntax_resolver(&inner_root, || {
                assert!(inner_id.to_node_from_root(&inner_root).is_some());
                assert!(outer_id.to_node_from_root(&outer_root).is_some());
                assert!(memo_has_root(&outer_root));
                assert!(memo_has_root(&inner_root));
                assert!(!memo_has_root(&equivalent_root));
            });
        });
    }

    #[test]
    fn resolver_memo_survives_panic() {
        let root = parsed_root("local value = 1\n");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_syntax_resolver(&root, || panic!("test resolver scope"));
        }));

        assert!(result.is_err());
        let id = LuaSyntaxId::from_node(&root.descendants().next().unwrap());
        assert!(id.to_node_from_root(&root).is_some());
    }

    #[test]
    fn resolver_memo_handles_roots_rebuilt_from_one_tree() {
        let tree = LuaParser::parse("local value = 1\n", ParserConfig::default());
        let first_root = tree.get_red_root();
        let second_root = tree.get_red_root();
        let id = LuaSyntaxId::from_node(&first_root.descendants().next().unwrap());

        with_syntax_resolvers(|| {
            assert!(id.to_node_from_root(&first_root).is_some());
            assert!(id.to_node_from_root(&second_root).is_some());
        });
    }

    #[test]
    fn resolver_memo_retains_roots_within_bound() {
        let roots = (0..8)
            .map(|index| parsed_root(&format!("local value{index} = {index}\n")))
            .collect::<Vec<_>>();
        with_syntax_resolvers(|| {
            for root in roots.iter().chain(roots.iter().rev()) {
                let id = LuaSyntaxId::from_node(&root.descendants().next().unwrap());
                assert!(id.to_node_from_root(root).is_some());
            }
            for root in &roots {
                assert_eq!(memo_entries(root), Some(1));
            }
        });
        assert!(memo_roots_len() <= roots.len());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LuaSyntaxId {
    kind: LuaKind,
    range: TextRange,
}

/// Source order: by range start, then range end, then kind.
impl Ord for LuaSyntaxId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.range.start(), self.range.end(), self.kind).cmp(&(
            other.range.start(),
            other.range.end(),
            other.kind,
        ))
    }
}

impl PartialOrd for LuaSyntaxId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl LuaSyntaxId {
    pub fn new(kind: LuaKind, range: TextRange) -> Self {
        LuaSyntaxId { kind, range }
    }

    pub fn from_ptr(ptr: LuaSyntaxNodePtr) -> Self {
        LuaSyntaxId {
            kind: ptr.kind(),
            range: ptr.text_range(),
        }
    }

    pub fn from_node(node: &LuaSyntaxNode) -> Self {
        LuaSyntaxId {
            kind: node.kind(),
            range: node.text_range(),
        }
    }

    pub fn from_token(token: &LuaSyntaxToken) -> Self {
        LuaSyntaxId {
            kind: token.kind(),
            range: token.text_range(),
        }
    }

    pub fn get_kind(&self) -> LuaSyntaxKind {
        self.kind.into()
    }

    pub fn get_token_kind(&self) -> LuaTokenKind {
        self.kind.into()
    }

    pub fn is_token(&self) -> bool {
        self.kind.is_token()
    }

    pub fn is_node(&self) -> bool {
        self.kind.is_syntax()
    }

    pub fn get_range(&self) -> TextRange {
        self.range
    }

    /// The same syntax kind at `range`.
    pub fn with_range(&self, range: TextRange) -> Self {
        LuaSyntaxId {
            kind: self.kind,
            range,
        }
    }

    pub fn to_node(&self, tree: &LuaSyntaxTree) -> Option<LuaSyntaxNode> {
        let root = tree.get_red_root();
        if root.parent().is_some() {
            return None;
        }
        self.to_node_from_root(&root)
    }

    /// Resolve this id to its node, memoized per thread: an uncached walk
    /// allocates a red node per nesting level.
    pub fn to_node_from_root(&self, root: &LuaSyntaxNode) -> Option<LuaSyntaxNode> {
        NODE_MEMO.with(|memo| memo.borrow_mut().resolve(*self, root))
    }

    fn walk_from_root(&self, root: &LuaSyntaxNode) -> Option<LuaSyntaxNode> {
        successors(Some(root.clone()), |node| {
            node.child_or_token_at_range(self.range)?.into_node()
        })
        .find(|it| it.text_range() == self.range && it.kind() == self.kind)
    }

    pub fn to_token(&self, tree: &LuaSyntaxTree) -> Option<LuaSyntaxToken> {
        let root = tree.get_red_root();
        if root.parent().is_some() {
            return None;
        }
        self.to_token_from_root(&root)
    }

    pub fn to_token_from_root(&self, root: &LuaSyntaxNode) -> Option<LuaSyntaxToken> {
        let mut current_node = Some(root.clone());
        while let Some(node) = current_node {
            let node_or_token = node.child_or_token_at_range(self.range)?;
            match node_or_token {
                rowan::NodeOrToken::Node(node) => {
                    current_node = Some(node);
                }
                rowan::NodeOrToken::Token(token) => {
                    if token.text_range() == self.range && token.kind() == self.kind {
                        return Some(token);
                    }
                    return None;
                }
            }
        }
        None
    }

    pub fn to_node_at_range(root: &LuaSyntaxNode, range: TextRange) -> Option<LuaSyntaxNode> {
        successors(Some(root.clone()), |node| {
            node.child_or_token_at_range(range)?.into_node()
        })
        .find(|it| it.text_range() == range)
    }
}

impl Serialize for LuaSyntaxId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let kind_raw = self.kind.get_raw();
        let start = u32::from(self.range.start());
        let end = u32::from(self.range.end());
        let range_combined = ((start as u64) << 32) | (end as u64);
        let value = format!("{:x}:{:x}", kind_raw, range_combined);
        serializer.serialize_str(&value)
    }
}

impl<'de> Deserialize<'de> for LuaSyntaxId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LuaSyntaxIdVisitor;

        impl Visitor<'_> for LuaSyntaxIdVisitor {
            type Value = LuaSyntaxId;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string with format 'kind:range'")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let parts: Vec<&str> = value.split(':').collect();
                if parts.len() != 2 {
                    return Err(E::custom("expected format 'kind:range'"));
                }

                let kind_raw = u16::from_str_radix(parts[0], 16)
                    .map_err(|e| E::custom(format!("invalid kind: {}", e)))?;
                let range_combined = u64::from_str_radix(parts[1], 16)
                    .map_err(|e| E::custom(format!("invalid range: {}", e)))?;

                let start = TextSize::new(((range_combined >> 32) & 0xFFFFFFFF) as u32);
                let end = TextSize::new((range_combined & 0xFFFFFFFF) as u32);

                Ok(LuaSyntaxId {
                    kind: LuaKind::from_raw(kind_raw),
                    range: TextRange::new(start, end),
                })
            }
        }

        deserializer.deserialize_str(LuaSyntaxIdVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LuaAstPtr<T: LuaAstNode> {
    pub syntax_id: LuaSyntaxId,
    _phantom: PhantomData<T>,
}

impl<T: LuaAstNode> LuaAstPtr<T> {
    pub fn new(node: &T) -> Self {
        LuaAstPtr {
            syntax_id: node.get_syntax_id(),
            _phantom: PhantomData,
        }
    }

    pub fn get_syntax_id(&self) -> LuaSyntaxId {
        self.syntax_id
    }

    pub fn to_node(&self, root: &LuaChunk) -> Option<T> {
        let syntax_node = self.syntax_id.to_node_from_root(root.syntax());
        if let Some(node) = syntax_node {
            T::cast(node)
        } else {
            None
        }
    }
}

unsafe impl<T: LuaAstNode> Send for LuaAstPtr<T> {}
unsafe impl<T: LuaAstNode> Sync for LuaAstPtr<T> {}
