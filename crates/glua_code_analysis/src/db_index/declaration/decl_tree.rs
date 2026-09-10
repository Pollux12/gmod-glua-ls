use rustc_hash::FxHashMap;
use std::collections::BTreeMap;

use super::{LuaDeclId, decl, scope};
use crate::{FileId, db_index::LuaMemberId};
use decl::LuaDecl;
use rowan::{TextRange, TextSize};
use scope::{LuaScope, LuaScopeId, LuaScopeKind, ScopeOrDeclId};
use smol_str::SmolStr;

#[derive(Debug)]
pub struct LuaDeclarationTree {
    file_id: FileId,
    decls: FxHashMap<LuaDeclId, LuaDecl>,
    module_decls_by_name: FxHashMap<String, BTreeMap<TextSize, LuaDeclId>>,
    scopes: Vec<LuaScope>,
    /// Each scope's index in its parent's `children`, parallel to `scopes`.
    scope_child_index: Vec<u32>,
    /// Per scope, the children `base_walk_up` would visit that can match:
    /// direct declarations, plus child scopes with direct declarations of
    /// their own. Sorted by `(position, child index)` — the same order they
    /// hold in `children` — so the walk iterates newest-first by
    /// binary-searching the `position < start` boundary instead of scanning
    /// every statement scope. Only scopes with any hold an entry.
    interesting_children: FxHashMap<LuaScopeId, Vec<((TextSize, u32), InterestingChild)>>,
    /// The same ordered visit set keyed by declaration name. `find_local_decl`
    /// only needs one name, so it can skip unrelated declarations while keeping
    /// the source-order walk and scope boundaries intact.
    interesting_children_by_name:
        FxHashMap<LuaScopeId, FxHashMap<SmolStr, Vec<((TextSize, u32), InterestingChild)>>>,
}

impl LuaDeclarationTree {
    pub fn new(file_id: FileId) -> Self {
        Self {
            file_id,
            decls: FxHashMap::default(),
            module_decls_by_name: FxHashMap::default(),
            scopes: Vec::new(),
            scope_child_index: Vec::new(),
            interesting_children: FxHashMap::default(),
            interesting_children_by_name: FxHashMap::default(),
        }
    }

    pub fn file_id(&self) -> FileId {
        self.file_id
    }

    pub fn find_local_decl(&self, name: &str, position: TextSize) -> Option<&LuaDecl> {
        let scope = self.find_scope(position)?;
        let mut result: Option<&LuaDecl> = None;
        self.walk_up(scope, position, 0, Some(name), &mut |decl_id| {
            match decl_id {
                ScopeOrDeclId::Decl(decl_id) => {
                    let decl = match self.get_decl(&decl_id) {
                        Some(decl) => decl,
                        None => return false,
                    };
                    if decl.get_name() == name {
                        result = Some(decl);
                        return true;
                    }
                }
                ScopeOrDeclId::Scope(_) => {}
            }

            false
        });
        result
    }

    pub fn find_module_scoped_decl_anywhere(
        &self,
        name: &str,
        module_path: &str,
        activation_position: TextSize,
    ) -> Option<&LuaDecl> {
        let positions = self.module_decls_by_name.get(name)?;
        positions
            .range((activation_position + TextSize::new(1))..)
            .find_map(|(_, decl_id)| {
                self.get_decl(decl_id).filter(|decl| {
                    decl.is_module_scoped() && decl.get_module_path() == Some(module_path)
                })
            })
    }

    pub fn get_env_decls(&self, position: TextSize) -> Option<Vec<LuaDeclId>> {
        let scope = self.find_scope(position)?;
        let mut result = Vec::new();
        self.walk_up(scope, position, 0, None, &mut |decl_id| {
            match decl_id {
                ScopeOrDeclId::Decl(decl_id) => {
                    if let Some(decl) = self.get_decl(&decl_id)
                        && !decl.is_implicit_self()
                    {
                        result.push(decl.get_id());
                    }
                }
                ScopeOrDeclId::Scope(_) => {}
            }

            false
        });
        Some(result)
    }

    fn find_scope(&self, position: TextSize) -> Option<&LuaScope> {
        if self.scopes.is_empty() {
            return None;
        }
        let mut scope = self.scopes.first()?;

        loop {
            let child_scope = self.find_child_scope(scope, position);
            if child_scope.is_none() {
                break;
            }
            scope = child_scope?;
        }

        Some(scope)
    }

    /// Innermost direct child scope of `scope` containing `position`, if any.
    ///
    /// Children are appended in walk (source) order and sibling scopes cover
    /// disjoint syntax ranges, so child starts are ordered and at most one
    /// sibling contains the position: binary search finds it instead of
    /// scanning every statement scope of the file on each lookup. An
    /// unresolvable key falls back to the linear scan; a candidate that does
    /// not contain the position means no sibling does (earlier ones end no
    /// later than its start), so that answers `None` directly. Debug builds
    /// check every answer against the linear scan.
    fn find_child_scope(&self, scope: &LuaScope, position: TextSize) -> Option<&LuaScope> {
        let children = scope.get_children();
        if children.is_empty() {
            return None;
        }
        let child_start = |child: &ScopeOrDeclId| -> Option<TextSize> {
            match child {
                ScopeOrDeclId::Scope(child_id) => self
                    .scopes
                    .get(child_id.id as usize)
                    .map(|child_scope| child_scope.get_position()),
                ScopeOrDeclId::Decl(decl_id) => {
                    self.get_decl(decl_id).map(|decl| decl.get_position())
                }
            }
        };

        // First index whose start lies past `position`. Children arrive in
        // walk (source) order, so starts are ordered and this is a binary
        // search; any unresolvable key falls back to the linear scan.
        let mut lo = 0usize;
        let mut hi = children.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match child_start(&children[mid]) {
                None => return self.linear_find_child_scope(scope, position),
                Some(start) if start <= position => lo = mid + 1,
                Some(_) => hi = mid,
            }
        }
        // The containing scope, if any, is the nearest preceding scope entry:
        // entries between it and `lo` are declarations, which never contain.
        // Scopes past `lo` start beyond `position` and cannot contain it, and
        // under the ordering invariant earlier scopes end no later than this
        // one's start, so a miss here means no sibling contains the position.
        let mut found: Option<&LuaScope> = None;
        let mut back = lo;
        while back > 0 {
            back -= 1;
            match &children[back] {
                ScopeOrDeclId::Decl(_) => continue,
                ScopeOrDeclId::Scope(child_id) => {
                    found = self.scopes.get(child_id.id as usize);
                    break;
                }
            }
        }
        let hit = found.filter(|candidate| candidate.get_range().contains(position));
        #[cfg(debug_assertions)]
        debug_assert_eq!(
            hit.map(|candidate| candidate.get_id()),
            self.linear_find_child_scope(scope, position)
                .map(|found| found.get_id()),
            "find_child_scope fast path diverged from the linear scan",
        );
        hit
    }

    /// Reference implementation of [`Self::find_child_scope`]'s inner step,
    /// kept to pin the fast path in debug builds and as the fallback for
    /// unresolvable keys in all builds.
    fn linear_find_child_scope(&self, scope: &LuaScope, position: TextSize) -> Option<&LuaScope> {
        scope
            .get_children()
            .iter()
            .filter_map(|child| match child {
                ScopeOrDeclId::Scope(child_id) => self.scopes.get(child_id.id as usize),
                ScopeOrDeclId::Decl(_) => None,
            })
            .find(|child_scope| child_scope.get_range().contains(position))
    }

    /// Walks up the scope tree, newest declaration first.
    ///
    /// Sibling scopes without direct declarations are elided: their only visit
    /// would be `f(Scope)`, which carries no declaration. Both current callers
    /// ignore `Scope` visits, so the elision is unobservable to them; a future
    /// closure that depends on visiting every scope must not use this walk.
    fn base_walk_up<F>(
        &self,
        scope: &LuaScope,
        start_pos: TextSize,
        level: usize,
        name: Option<&str>,
        f: &mut F,
    ) where
        F: FnMut(ScopeOrDeclId) -> bool,
    {
        let interesting = match name {
            Some(name) => self
                .interesting_children_by_name
                .get(&scope.get_id())
                .and_then(|children| children.get(name)),
            None => self.interesting_children.get(&scope.get_id()),
        };
        if let Some(interesting) = interesting {
            // Sorted by `(position, child index)`, the order entries hold in
            // `children`, so this visits exactly what the old reverse scan
            // over `children` would, minus the elided scopes.
            let end = interesting.partition_point(|(key, _)| key.0 < start_pos);
            for (_, child) in interesting[..end].iter().rev() {
                match child {
                    InterestingChild::Decl(decl_id) => {
                        if f((*decl_id).into()) {
                            return;
                        }
                    }
                    InterestingChild::Scope(scope_id) => {
                        let Some(child_scope) = self.scopes.get(scope_id.id as usize) else {
                            continue;
                        };
                        if self.walk_over_scope(child_scope, f) {
                            return;
                        }
                    }
                }
            }
        }

        if let Some(parent_id) = scope.get_parent() {
            let parent_scope = match self.scopes.get(parent_id.id as usize) {
                Some(scope) => scope,
                None => return,
            };
            self.walk_up(parent_scope, start_pos, level + 1, name, f);
        }
    }

    /// Walks up the scope tree and calls `f` for each declaration.
    fn walk_up<F>(
        &self,
        scope: &LuaScope,
        start_pos: TextSize,
        level: usize,
        name: Option<&str>,
        f: &mut F,
    ) where
        F: FnMut(ScopeOrDeclId) -> bool,
    {
        match scope.get_kind() {
            LuaScopeKind::LocalOrAssignStat => {
                let parent = scope.get_parent();
                if let Some(parent) = parent {
                    let parent_scope = match self.scopes.get(parent.id as usize) {
                        Some(scope) => scope,
                        None => return,
                    };
                    self.walk_up(parent_scope, scope.get_position(), level, name, f);
                }
            }
            LuaScopeKind::Repeat => {
                if level == 0
                    && let Some(ScopeOrDeclId::Scope(scope_id)) = scope.get_children().first()
                {
                    let scope = match self.scopes.get(scope_id.id as usize) {
                        Some(scope) => scope,
                        None => return,
                    };
                    self.walk_up(scope, start_pos, level, name, f);
                    return;
                }

                self.base_walk_up(scope, start_pos, level, name, f);
            }
            LuaScopeKind::ForRange => {
                if level == 0 {
                    let parent = scope.get_parent();
                    if let Some(parent) = parent {
                        let parent_scope = match self.scopes.get(parent.id as usize) {
                            Some(scope) => scope,
                            None => return,
                        };
                        self.walk_up(parent_scope, start_pos, level, name, f);
                    }
                } else {
                    self.base_walk_up(scope, start_pos, level, name, f);
                }
            }
            _ => {
                self.base_walk_up(scope, start_pos, level, name, f);
            }
        }
    }

    fn walk_over_scope<F>(&self, scope: &LuaScope, f: &mut F) -> bool
    where
        F: FnMut(ScopeOrDeclId) -> bool,
    {
        match scope.get_kind() {
            LuaScopeKind::LocalOrAssignStat | LuaScopeKind::FuncStat | LuaScopeKind::MethodStat => {
                for child in scope.get_children() {
                    if let ScopeOrDeclId::Decl(decl_id) = child
                        && f(decl_id.into())
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }

        f(scope.get_id().into())
    }

    pub fn add_decl(&mut self, decl: LuaDecl) -> LuaDeclId {
        if decl.is_module_scoped() {
            self.module_decls_by_name
                .entry(decl.get_name().to_string())
                .or_default()
                .insert(decl.get_position(), decl.get_id());
        }
        let decl_id = decl.get_id();
        self.decls.insert(decl_id, decl);
        decl_id
    }

    #[allow(unused)]
    pub fn get_decl_mut(&mut self, decl_id: LuaDeclId) -> Option<&mut LuaDecl> {
        self.decls.get_mut(&decl_id)
    }

    pub fn get_decl(&self, decl_id: &LuaDeclId) -> Option<&LuaDecl> {
        self.decls.get(decl_id)
    }

    pub fn create_scope(&mut self, range: TextRange, kind: LuaScopeKind) -> LuaScopeId {
        let id = self.scopes.len() as u32;
        let scope_id = LuaScopeId {
            file_id: self.file_id,
            id,
        };

        let scope = LuaScope::new(range, kind, scope_id);
        self.scopes.push(scope);
        self.scope_child_index.push(u32::MAX);
        scope_id
    }

    pub fn add_decl_to_scope(&mut self, scope_id: LuaScopeId, decl_id: LuaDeclId) {
        let decl_name = self
            .get_decl(&decl_id)
            .map(|decl| SmolStr::new(decl.get_name()));
        let Some(scope) = self.scopes.get_mut(scope_id.id as usize) else {
            return;
        };
        let vec_index = scope.get_children().len() as u32;
        scope.add_decl(decl_id);
        let (kind, parent, position) = (scope.get_kind(), scope.get_parent(), scope.get_position());
        insert_interesting_child(
            &mut self.interesting_children,
            scope_id,
            (decl_id.position, vec_index),
            InterestingChild::Decl(decl_id),
        );
        if let Some(name) = decl_name.as_deref() {
            insert_named_interesting_child(
                &mut self.interesting_children_by_name,
                scope_id,
                name,
                (decl_id.position, vec_index),
                InterestingChild::Decl(decl_id),
            );
        }
        // A child scope with direct declarations is visited for them when the
        // walk passes its parent; record it there. The key makes repeat
        // inserts idempotent, so every declaration can file unconditionally.
        if matches!(
            kind,
            LuaScopeKind::LocalOrAssignStat | LuaScopeKind::FuncStat | LuaScopeKind::MethodStat
        ) && let Some(parent_id) = parent
        {
            let scope_key = (
                position,
                self.scope_child_index
                    .get(scope_id.id as usize)
                    .copied()
                    .unwrap_or(u32::MAX),
            );
            insert_interesting_child(
                &mut self.interesting_children,
                parent_id,
                scope_key,
                InterestingChild::Scope(scope_id),
            );
            if let Some(name) = decl_name.as_deref() {
                insert_named_interesting_child(
                    &mut self.interesting_children_by_name,
                    parent_id,
                    name,
                    scope_key,
                    InterestingChild::Scope(scope_id),
                );
            }
        }
    }

    pub fn add_child_scope(&mut self, parent_id: LuaScopeId, child_id: LuaScopeId) {
        if let Some(parent) = self.scopes.get_mut(parent_id.id as usize) {
            let vec_index = parent.get_children().len() as u32;
            parent.add_child(child_id);
            if let Some(slot) = self.scope_child_index.get_mut(child_id.id as usize) {
                *slot = vec_index;
            }
        }
        if let Some(child) = self.scopes.get_mut(child_id.id as usize) {
            child.set_parent(Some(parent_id));
        }
        // Declarations normally land after the link above, which files the
        // scope into its parent then. If any landed first, the parent never
        // saw them: file the scope now so the walk finds them either way.
        self.file_child_scope_with_decls(parent_id, child_id);
    }

    /// Records `child_id` under `parent_id`'s visit set when the child
    /// already holds direct declarations (see `add_decl_to_scope` for the
    /// usual link-then-declare order this covers for).
    fn file_child_scope_with_decls(&mut self, parent_id: LuaScopeId, child_id: LuaScopeId) {
        let (kind, position, vec_index, has_decls, names) =
            match self.scopes.get(child_id.id as usize) {
                Some(child) => (
                    child.get_kind(),
                    child.get_position(),
                    self.scope_child_index
                        .get(child_id.id as usize)
                        .copied()
                        .unwrap_or(u32::MAX),
                    child
                        .get_children()
                        .iter()
                        .any(|entry| matches!(entry, ScopeOrDeclId::Decl(_))),
                    child
                        .get_children()
                        .iter()
                        .filter_map(|entry| match entry {
                            ScopeOrDeclId::Decl(decl_id) => self
                                .get_decl(decl_id)
                                .map(|decl| SmolStr::new(decl.get_name())),
                            ScopeOrDeclId::Scope(_) => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                None => return,
            };
        if !has_decls
            || !matches!(
                kind,
                LuaScopeKind::LocalOrAssignStat | LuaScopeKind::FuncStat | LuaScopeKind::MethodStat
            )
        {
            return;
        }
        insert_interesting_child(
            &mut self.interesting_children,
            parent_id,
            (position, vec_index),
            InterestingChild::Scope(child_id),
        );
        for name in names {
            insert_named_interesting_child(
                &mut self.interesting_children_by_name,
                parent_id,
                &name,
                (position, vec_index),
                InterestingChild::Scope(child_id),
            );
        }
    }

    pub fn get_root_scope(&self) -> Option<&LuaScope> {
        self.scopes.first()
    }

    pub fn get_scope(&self, scope_id: &LuaScopeId) -> Option<&LuaScope> {
        self.scopes.get(scope_id.id as usize)
    }

    pub fn get_decls(&self) -> &FxHashMap<LuaDeclId, LuaDecl> {
        &self.decls
    }
}

/// A child `base_walk_up` visits that can match: a direct declaration, or a
/// child scope visited for its own direct declarations via `walk_over_scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterestingChild {
    Decl(LuaDeclId),
    Scope(LuaScopeId),
}

/// Files `child` under `scope_id`'s ordered visit set, keeping `(position,
/// child index)` order. Scope entries deduplicate by key, so a scope files
/// once however many declarations it gains.
fn insert_interesting_child(
    interesting: &mut FxHashMap<LuaScopeId, Vec<((TextSize, u32), InterestingChild)>>,
    scope_id: LuaScopeId,
    key: (TextSize, u32),
    child: InterestingChild,
) {
    let entries = interesting.entry(scope_id).or_default();
    match entries.binary_search_by_key(&key, |(key, _)| *key) {
        // Same key twice is the same scope filing again; declarations never
        // share a key (their child index is unique per push).
        Ok(_) if matches!(child, InterestingChild::Scope(_)) => {}
        Ok(at) | Err(at) => entries.insert(at, (key, child)),
    }
}

fn insert_named_interesting_child(
    interesting: &mut FxHashMap<
        LuaScopeId,
        FxHashMap<SmolStr, Vec<((TextSize, u32), InterestingChild)>>,
    >,
    scope_id: LuaScopeId,
    name: &str,
    key: (TextSize, u32),
    child: InterestingChild,
) {
    let entries = interesting
        .entry(scope_id)
        .or_default()
        .entry(SmolStr::new(name))
        .or_default();
    match entries.binary_search_by_key(&key, |(key, _)| *key) {
        // Child scopes may be filed once per direct declaration; preserve one
        // scope entry for each `(position, child index)` key.
        Ok(_) if matches!(child, InterestingChild::Scope(_)) => {}
        Ok(at) | Err(at) => entries.insert(at, (key, child)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LuaDeclOrMemberId {
    Decl(LuaDeclId),
    Member(LuaMemberId),
}

impl LuaDeclOrMemberId {
    pub fn as_decl_id(&self) -> Option<LuaDeclId> {
        match self {
            LuaDeclOrMemberId::Decl(decl_id) => Some(*decl_id),
            LuaDeclOrMemberId::Member(_) => None,
        }
    }

    pub fn as_member_id(&self) -> Option<LuaMemberId> {
        match self {
            LuaDeclOrMemberId::Decl(_) => None,
            LuaDeclOrMemberId::Member(member_id) => Some(*member_id),
        }
    }

    pub fn get_position(&self) -> TextSize {
        match self {
            LuaDeclOrMemberId::Decl(decl_id) => decl_id.position,
            LuaDeclOrMemberId::Member(member_id) => member_id.get_position(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileId, LuaDeclExtra};
    use glua_parser::LuaSyntaxKind;

    fn range(start: u32, end: u32) -> TextRange {
        TextRange::new(TextSize::new(start), TextSize::new(end))
    }

    fn add_local(tree: &mut LuaDeclarationTree, name: &str, start: u32) -> LuaDeclId {
        tree.add_decl(LuaDecl::new(
            name,
            FileId::new(1),
            range(start, start + 1),
            LuaDeclExtra::Local {
                kind: LuaSyntaxKind::LocalName.into(),
                attrib: None,
            },
            None,
        ))
    }

    fn found_position(tree: &LuaDeclarationTree, name: &str, position: u32) -> Option<u32> {
        tree.find_local_decl(name, TextSize::new(position))
            .map(|decl| decl.get_position().into())
    }

    #[test]
    fn finds_latest_same_name_declaration_before_use() {
        let file_id = FileId::new(1);
        let mut tree = LuaDeclarationTree::new(file_id);
        let root = tree.create_scope(range(0, 100), LuaScopeKind::Normal);
        let first = add_local(&mut tree, "value", 10);
        tree.add_decl_to_scope(root, first);
        let other = add_local(&mut tree, "other", 20);
        tree.add_decl_to_scope(root, other);
        let second = add_local(&mut tree, "value", 30);
        tree.add_decl_to_scope(root, second);

        assert_eq!(found_position(&tree, "value", 10), None);
        assert_eq!(found_position(&tree, "value", 20), Some(10));
        assert_eq!(found_position(&tree, "value", 40), Some(30));
    }

    #[test]
    fn files_child_declarations_regardless_of_link_order() {
        let file_id = FileId::new(1);
        let mut tree = LuaDeclarationTree::new(file_id);
        let root = tree.create_scope(range(0, 200), LuaScopeKind::Normal);

        let linked_first = tree.create_scope(range(10, 40), LuaScopeKind::FuncStat);
        tree.add_child_scope(root, linked_first);
        let first_decl = add_local(&mut tree, "value", 20);
        tree.add_decl_to_scope(linked_first, first_decl);

        let declared_first = tree.create_scope(range(50, 80), LuaScopeKind::FuncStat);
        let second_decl = add_local(&mut tree, "value", 60);
        tree.add_decl_to_scope(declared_first, second_decl);
        tree.add_child_scope(root, declared_first);

        assert_eq!(found_position(&tree, "value", 100), Some(60));
    }

    #[test]
    fn preserves_direct_child_declaration_insertion_order() {
        let file_id = FileId::new(1);
        let mut tree = LuaDeclarationTree::new(file_id);
        let root = tree.create_scope(range(0, 200), LuaScopeKind::Normal);
        let child = tree.create_scope(range(20, 80), LuaScopeKind::FuncStat);
        tree.add_child_scope(root, child);

        // The declaration walker files children in traversal order. Keep that
        // order even when their source positions arrive out of order.
        let later = add_local(&mut tree, "value", 60);
        tree.add_decl_to_scope(child, later);
        let earlier = add_local(&mut tree, "value", 30);
        tree.add_decl_to_scope(child, earlier);

        assert_eq!(found_position(&tree, "value", 100), Some(60));
    }

    #[test]
    fn local_assignment_scope_hides_its_declaration_from_its_rhs() {
        let file_id = FileId::new(1);
        let mut tree = LuaDeclarationTree::new(file_id);
        let root = tree.create_scope(range(0, 100), LuaScopeKind::Normal);
        let local_scope = tree.create_scope(range(20, 40), LuaScopeKind::LocalOrAssignStat);
        tree.add_child_scope(root, local_scope);
        let decl = add_local(&mut tree, "value", 30);
        tree.add_decl_to_scope(local_scope, decl);

        assert_eq!(found_position(&tree, "value", 25), None);
        assert_eq!(found_position(&tree, "value", 50), Some(30));
    }

    #[test]
    fn for_range_declaration_is_visible_in_body_after_header() {
        let file_id = FileId::new(1);
        let mut tree = LuaDeclarationTree::new(file_id);
        let root = tree.create_scope(range(0, 200), LuaScopeKind::Normal);
        let for_scope = tree.create_scope(range(20, 150), LuaScopeKind::ForRange);
        tree.add_child_scope(root, for_scope);
        let body = tree.create_scope(range(50, 140), LuaScopeKind::Normal);
        tree.add_child_scope(for_scope, body);
        let decl = add_local(&mut tree, "value", 25);
        tree.add_decl_to_scope(for_scope, decl);

        assert_eq!(found_position(&tree, "value", 30), None);
        assert_eq!(found_position(&tree, "value", 60), Some(25));
    }
}
