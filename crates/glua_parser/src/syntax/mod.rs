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

/// Maximum number of resolved nodes retained by the compatibility memo for one
/// root. Scoped resolvers keep their entries only until their scope ends.
const MAX_ENTRIES_PER_ROOT: usize = 8192;

/// A resolver for one red syntax root. Keeping the root alongside its
/// nodes makes identity comparison sound for the resolver's lifetime.
#[derive(Debug)]
pub struct LuaSyntaxResolver {
    root: LuaSyntaxNode,
    entries: rustc_hash::FxHashMap<LuaSyntaxId, Option<LuaSyntaxNode>>,
    bounded: bool,
}

impl LuaSyntaxResolver {
    /// Creates a resolver whose cache is released when the resolver is dropped.
    pub fn new(root: LuaSyntaxNode) -> Self {
        Self {
            root,
            entries: rustc_hash::FxHashMap::default(),
            bounded: false,
        }
    }

    fn bounded(root: LuaSyntaxNode) -> Self {
        Self {
            root,
            entries: rustc_hash::FxHashMap::default(),
            bounded: true,
        }
    }

    /// Resolves an id against this resolver's root, retaining the result.
    pub fn resolve(&mut self, id: LuaSyntaxId) -> Option<LuaSyntaxNode> {
        if let Some(hit) = self.entries.get(&id) {
            return hit.clone();
        }

        let resolved = id.walk_from_root(&self.root);
        if self.bounded && self.entries.len() >= MAX_ENTRIES_PER_ROOT {
            self.entries.clear();
        }
        self.entries.insert(id, resolved.clone());
        resolved
    }
}

/// Per-thread memo for [`LuaSyntaxId::to_node_from_root`], keyed by root
/// (MRU, a few roots kept). Holding each root alive keeps its green tree
/// alive, which is what makes identity comparison sound.
mod node_memo {
    use super::{LuaSyntaxId, LuaSyntaxNode, LuaSyntaxResolver};

    const MAX_ROOTS: usize = 4;

    #[derive(Default)]
    pub(super) struct NodeMemo {
        roots: Vec<LuaSyntaxResolver>,
    }

    impl NodeMemo {
        pub(super) fn resolve(
            &mut self,
            id: LuaSyntaxId,
            root: &LuaSyntaxNode,
        ) -> Option<LuaSyntaxNode> {
            let found = self.roots.iter().position(|cached| cached.root == *root);
            let index = match found {
                Some(0) => 0,
                Some(index) => {
                    self.roots.swap(0, index);
                    0
                }
                None => {
                    if self.roots.len() == MAX_ROOTS {
                        self.roots.pop();
                    }
                    self.roots
                        .insert(0, LuaSyntaxResolver::bounded(root.clone()));
                    0
                }
            };

            self.roots[index].resolve(id)
        }
    }
}

thread_local! {
    static NODE_MEMO: std::cell::RefCell<node_memo::NodeMemo> =
        std::cell::RefCell::new(node_memo::NodeMemo::default());
    static SCOPED_RESOLVERS: std::cell::RefCell<Vec<ScopedResolvers>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static SCOPED_RESOLVERS_ACTIVE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[derive(Default)]
struct ScopedResolvers {
    resolvers: rustc_hash::FxHashMap<LuaSyntaxNode, LuaSyntaxResolver>,
}

impl ScopedResolvers {
    fn new(root: Option<&LuaSyntaxNode>) -> Self {
        Self {
            resolvers: root
                .map(|root| (root.clone(), LuaSyntaxResolver::new(root.clone())))
                .into_iter()
                .collect(),
        }
    }

    fn resolve_existing(
        &mut self,
        id: LuaSyntaxId,
        root: &LuaSyntaxNode,
    ) -> Option<Option<LuaSyntaxNode>> {
        self.resolvers
            .get_mut(root)
            .map(|resolver| resolver.resolve(id))
    }

    fn resolve_new(
        &mut self,
        id: LuaSyntaxId,
        root: &LuaSyntaxNode,
    ) -> Option<Option<LuaSyntaxNode>> {
        Some(
            self.resolvers
                .entry(root.clone())
                .or_insert_with(|| LuaSyntaxResolver::new(root.clone()))
                .resolve(id),
        )
    }
}

fn with_scoped_resolvers<R>(root: Option<&LuaSyntaxNode>, f: impl FnOnce() -> R) -> R {
    SCOPED_RESOLVERS.with(|resolvers| {
        resolvers.borrow_mut().push(ScopedResolvers::new(root));
    });
    let was_active = SCOPED_RESOLVERS_ACTIVE.with(|active| active.replace(true));

    struct Scope {
        was_active: bool,
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            SCOPED_RESOLVERS.with(|resolvers| {
                resolvers.borrow_mut().pop();
            });
            SCOPED_RESOLVERS_ACTIVE.with(|active| active.set(self.was_active));
        }
    }

    let _scope = Scope { was_active };
    f()
}

/// Runs `f` with lazily created root resolvers scoped to the current thread.
///
/// Use this when the callback creates its own red roots. Each root is retained
/// only until the scope ends.
pub fn with_syntax_resolvers<R>(f: impl FnOnce() -> R) -> R {
    with_scoped_resolvers(None, f)
}

/// Runs `f` with a resolver scoped to `root` on the current thread.
///
/// Nested scopes select the innermost matching root and are removed on normal
/// or unwinding exit.
pub fn with_syntax_resolver<R>(root: &LuaSyntaxNode, f: impl FnOnce() -> R) -> R {
    with_scoped_resolvers(Some(root), f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LuaParser, ParserConfig};

    fn parsed_root(source: &str) -> LuaSyntaxNode {
        LuaParser::parse(source, ParserConfig::default()).get_red_root()
    }

    #[test]
    fn scoped_resolver_routes_matching_root_and_drops_after_scope() {
        let root = parsed_root("local value = 1\n");
        let id = LuaSyntaxId::from_node(&root.descendants().next().unwrap());

        with_syntax_resolver(&root, || {
            assert!(id.to_node_from_root(&root).is_some());
            SCOPED_RESOLVERS.with(|scopes| {
                assert_eq!(scopes.borrow()[0].resolvers[&root].entries.len(), 1);
            });
        });

        SCOPED_RESOLVERS.with(|resolvers| assert!(resolvers.borrow().is_empty()));
    }

    #[test]
    fn scoped_resolver_supports_nested_roots_and_exact_matching() {
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
                SCOPED_RESOLVERS.with(|scopes| {
                    let scopes = scopes.borrow();
                    assert_eq!(scopes.len(), 2);
                    assert_eq!(scopes[0].resolvers.len(), 1);
                    assert_eq!(scopes[1].resolvers.len(), 1);
                    assert!(!scopes[0].resolvers.contains_key(&equivalent_root));
                });
            });
            SCOPED_RESOLVERS.with(|resolvers| assert_eq!(resolvers.borrow().len(), 1));
        });
    }

    #[test]
    fn scoped_resolver_drops_after_panic() {
        let root = parsed_root("local value = 1\n");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_syntax_resolver(&root, || panic!("test resolver scope"));
        }));

        assert!(result.is_err());
        SCOPED_RESOLVERS.with(|resolvers| assert!(resolvers.borrow().is_empty()));
    }

    #[test]
    fn lazy_scoped_resolver_handles_roots_rebuilt_from_one_tree() {
        let tree = LuaParser::parse("local value = 1\n", ParserConfig::default());
        let first_root = tree.get_red_root();
        let second_root = tree.get_red_root();
        let id = LuaSyntaxId::from_node(&first_root.descendants().next().unwrap());

        with_syntax_resolvers(|| {
            assert!(id.to_node_from_root(&first_root).is_some());
            assert!(id.to_node_from_root(&second_root).is_some());
        });

        SCOPED_RESOLVERS.with(|resolvers| assert!(resolvers.borrow().is_empty()));
    }

    #[test]
    fn lazy_scope_retains_each_tree_until_the_phase_ends() {
        let roots = (0..8)
            .map(|index| parsed_root(&format!("local value{index} = {index}\n")))
            .collect::<Vec<_>>();
        with_syntax_resolvers(|| {
            for root in roots.iter().chain(roots.iter().rev()) {
                let id = LuaSyntaxId::from_node(&root.descendants().next().unwrap());
                assert!(id.to_node_from_root(root).is_some());
            }
            SCOPED_RESOLVERS.with(|scopes| {
                let scopes = scopes.borrow();
                assert_eq!(scopes[0].resolvers.len(), roots.len());
                assert!(
                    scopes[0]
                        .resolvers
                        .values()
                        .all(|resolver| resolver.entries.len() == 1)
                );
            });
        });
        SCOPED_RESOLVERS.with(|scopes| assert!(scopes.borrow().is_empty()));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LuaSyntaxId {
    kind: LuaKind,
    range: TextRange,
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
        if SCOPED_RESOLVERS_ACTIVE.with(std::cell::Cell::get) {
            let scoped_result = SCOPED_RESOLVERS.with(|scopes| {
                let mut scopes = scopes.borrow_mut();
                for scope in scopes.iter_mut().rev() {
                    if let Some(result) = scope.resolve_existing(*self, root) {
                        return Some(result);
                    }
                }
                scopes.last_mut()?.resolve_new(*self, root)
            });
            if let Some(result) = scoped_result {
                return result;
            }
        }
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
