use glua_parser::{LuaAstNode, LuaKind, LuaSyntaxId, LuaTableExpr};
use rowan::{TextRange, TextSize};
use rustc_hash::FxHashSet;

use crate::{
    DbIndex, FileId, InFiled, LuaDeclId, LuaDefinitionId, LuaInferenceNodeId,
    LuaInferredGuardOwner, LuaMemberId, LuaOperatorOwner, LuaSemanticDeclId, LuaSignatureId,
    LuaTypeOwner,
};

/// The single hunk an edit changed, as byte offsets.
///
/// Offsets before `hunk_start` are unmoved; offsets at or after `old_hunk_end`
/// shift by `new_hunk_end - old_hunk_end`; offsets inside the hunk have no
/// image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionMap {
    hunk_start: u32,
    old_hunk_end: u32,
    new_hunk_end: u32,
}

impl PositionMap {
    /// The map for "nothing moved": every offset is its own image.
    pub fn identity() -> Self {
        Self {
            hunk_start: u32::MAX,
            old_hunk_end: u32::MAX,
            new_hunk_end: u32::MAX,
        }
    }

    /// The single hunk between two versions of a file's text.
    pub fn new(old_text: &str, new_text: &str) -> Self {
        let old = old_text.as_bytes();
        let new = new_text.as_bytes();
        let max_prefix = old.len().min(new.len());
        let mut prefix = 0;
        while prefix < max_prefix && old[prefix] == new[prefix] {
            prefix += 1;
        }
        // A prefix that ends mid-character would put a hunk boundary inside a
        // code point, and every offset the index holds is a character
        // boundary.
        while prefix > 0
            && !(old_text.is_char_boundary(prefix) && new_text.is_char_boundary(prefix))
        {
            prefix -= 1;
        }

        let max_suffix = (old.len() - prefix).min(new.len() - prefix);
        let mut suffix = 0;
        while suffix < max_suffix && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
            suffix += 1;
        }
        while suffix > 0
            && !(old_text.is_char_boundary(old.len() - suffix)
                && new_text.is_char_boundary(new.len() - suffix))
        {
            suffix -= 1;
        }

        let map = Self {
            hunk_start: prefix as u32,
            old_hunk_end: (old.len() - suffix) as u32,
            new_hunk_end: (new.len() - suffix) as u32,
        };
        debug_assert!(map.hunk_start <= map.old_hunk_end);
        debug_assert!(map.hunk_start <= map.new_hunk_end);
        map
    }

    /// The map for a file whose whole text was replaced, or that was deleted
    /// (`new_len == 0`). Nothing inside it survives.
    pub fn whole_file(old_len: usize, new_len: usize) -> Self {
        Self {
            hunk_start: 0,
            old_hunk_end: old_len as u32,
            new_hunk_end: new_len as u32,
        }
    }

    /// Whether every offset maps to itself, so the remap pass can be skipped.
    pub fn is_identity(&self) -> bool {
        self.hunk_start == self.old_hunk_end && self.old_hunk_end == self.new_hunk_end
    }

    /// The hunk as `(start, old_end, new_end)`.
    pub fn hunk(&self) -> (u32, u32, u32) {
        (self.hunk_start, self.old_hunk_end, self.new_hunk_end)
    }

    pub fn map(&self, pos: TextSize) -> Option<TextSize> {
        let p = u32::from(pos);
        if p < self.hunk_start {
            Some(pos)
        } else if p >= self.old_hunk_end {
            Some(TextSize::new(p - self.old_hunk_end + self.new_hunk_end))
        } else {
            None
        }
    }

    /// The image of a range, or `None` when an endpoint fell inside the hunk.
    ///
    /// A range whose endpoints straddle the hunk keeps its start and takes the
    /// shift on its end: the edit happened *within* it. Reporting it lost
    /// instead would invalidate every dependent of a hub table literal or a
    /// long function on any edit inside it, while what actually changed is
    /// visible as a per-member or per-signature export change.
    pub fn map_range(&self, range: TextRange) -> Option<TextRange> {
        let start = u32::from(range.start());
        let end = u32::from(range.end());
        if end <= self.hunk_start {
            return Some(range);
        }
        if start >= self.old_hunk_end {
            return Some(TextRange::new(
                self.map(range.start())?,
                self.map(range.end())?,
            ));
        }
        if start <= self.hunk_start && end >= self.old_hunk_end {
            return Some(TextRange::new(
                range.start(),
                TextSize::new(end - self.old_hunk_end + self.new_hunk_end),
            ));
        }
        None
    }
}

/// The three answers a remap can give about one stored identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remap<T> {
    /// The identity does not belong to the edited file. Leave the entry alone.
    Unrelated,
    /// The identity's new form. May equal the old one when the edit was below
    /// it.
    Moved(T),
    /// The identity did not survive the edit. The entry cannot be rewritten
    /// and its owning file is dirty.
    Lost,
}

impl<T> Remap<T> {
    pub fn is_lost(&self) -> bool {
        matches!(self, Self::Lost)
    }

    pub fn moved(self) -> Option<T> {
        match self {
            Self::Moved(value) => Some(value),
            _ => None,
        }
    }
}

/// What the edited file holds *after* it was re-indexed.
///
/// A [`PositionMap`] says where an offset would land; it cannot say whether
/// anything is still there. An edit that replaces one function with another of
/// the same length maps every identity onto a position that exists but names a
/// different thing, so every mapped identity is confirmed against these before
/// it is written back.
#[derive(Debug, Default)]
pub struct FileFacts {
    signature_positions: FxHashSet<TextSize>,
    decl_positions: FxHashSet<TextSize>,
    member_ids: FxHashSet<LuaSyntaxId>,
    syntax_ids: FxHashSet<LuaSyntaxId>,
    /// The ranges something can still own members on: a table literal in the
    /// new tree, or a range the re-indexed file registers an `Element` owner
    /// at. An owner is keyed by whatever expression the write was made on - a
    /// literal, but also a `vgui.Create(..)` call - so "is there still an
    /// owner here" is not a question the tree alone answers. Answers "did the
    /// mapped range land on something that still holds members".
    owner_ranges: FxHashSet<TextRange>,
}

impl FileFacts {
    pub fn collect(db: &DbIndex, file_id: FileId) -> Self {
        let signature_positions = db
            .get_signature_index()
            .get_file_signature_ids(file_id)
            .map(|ids| ids.iter().map(|id| id.get_position()).collect())
            .unwrap_or_default();
        let decl_positions = db
            .get_decl_index()
            .get_decl_tree(&file_id)
            .map(|tree| tree.get_decls().keys().map(|id| id.position).collect())
            .unwrap_or_default();
        let member_ids = db
            .get_member_index()
            .get_file_members(file_id)
            .into_iter()
            .map(|member| *member.get_id().get_syntax_id())
            .collect();

        let mut owner_ranges = db
            .get_member_index()
            .element_owner_ranges_in_file(file_id)
            .into_iter()
            .map(|range| range.value)
            .collect::<FxHashSet<_>>();
        let mut syntax_ids = FxHashSet::default();
        if let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) {
            for element in tree.get_red_root().descendants_with_tokens() {
                match element {
                    rowan::NodeOrToken::Node(node) => {
                        syntax_ids.insert(LuaSyntaxId::from_node(&node));
                    }
                    rowan::NodeOrToken::Token(token) => {
                        syntax_ids.insert(LuaSyntaxId::from_token(&token));
                    }
                }
            }
            owner_ranges.extend(
                tree.get_chunk_node()
                    .descendants::<LuaTableExpr>()
                    .map(|table| table.get_range()),
            );
        }

        Self {
            signature_positions,
            decl_positions,
            member_ids,
            syntax_ids,
            owner_ranges,
        }
    }
}

/// A [`PositionMap`] bound to the file it describes, so a caller cannot apply
/// it to an identity from another file.
#[derive(Debug)]
pub struct FileRemap {
    pub file_id: FileId,
    pub map: PositionMap,
    /// Absent while the new index does not exist yet - the pre-edit export map
    /// is built before the file is re-indexed, and an identity that maps onto
    /// nothing simply reads as a removed export there.
    facts: Option<FileFacts>,
}

impl FileRemap {
    /// A remap that moves nothing, for reading a file that was not edited.
    pub fn identity(file_id: FileId) -> Self {
        Self {
            file_id,
            map: PositionMap::identity(),
            facts: None,
        }
    }

    /// A remap whose results are taken on trust, for use before the file has
    /// been re-indexed.
    pub fn unvalidated(file_id: FileId, map: PositionMap) -> Self {
        Self {
            file_id,
            map,
            facts: None,
        }
    }

    /// A remap that confirms every mapped identity against the file's current
    /// index and tree. Use after `update_index(file_id)`.
    pub fn validated(db: &DbIndex, file_id: FileId, map: PositionMap) -> Self {
        Self {
            file_id,
            map,
            facts: (!map.is_identity()).then(|| FileFacts::collect(db, file_id)),
        }
    }

    fn owns(&self, file_id: FileId) -> bool {
        file_id == self.file_id
    }

    fn pos(&self, file_id: FileId, position: TextSize) -> Remap<TextSize> {
        if !self.owns(file_id) {
            return Remap::Unrelated;
        }
        match self.map.map(position) {
            Some(mapped) => Remap::Moved(mapped),
            None => Remap::Lost,
        }
    }

    fn range(&self, file_id: FileId, range: TextRange) -> Remap<TextRange> {
        if !self.owns(file_id) {
            return Remap::Unrelated;
        }
        match self.map.map_range(range) {
            Some(mapped) => Remap::Moved(mapped),
            None => Remap::Lost,
        }
    }

    /// Whether the position sits entirely in the edit's unchanged prefix.
    ///
    /// `map` returns an offset unchanged exactly when it lies before the
    /// hunk, and those bytes are identical in both texts, so the code the
    /// identity names is still there whatever the index happens to enumerate.
    /// Checking existence for it would reject live identities the facts
    /// cannot see - a `DocTagClass` node, say, which the syntax tree's
    /// `descendants_with_tokens` walk does not reach. Judged by geometry, not
    /// by whether the map happens to return the original value: a replacement
    /// that swaps one function for another at the same offset also maps the
    /// offset to itself, and that identity is *not* in the unchanged prefix.
    fn pos_is_before_the_hunk(&self, position: TextSize) -> bool {
        u32::from(position) < self.map.hunk_start
    }

    /// The range counterpart: `map_range` keeps a range unchanged exactly
    /// when its end lies at or before the hunk start.
    fn range_is_before_the_hunk(&self, range: TextRange) -> bool {
        u32::from(range.end()) <= self.map.hunk_start
    }

    /// Resolves a mapped identity against the re-indexed file.
    ///
    /// An identity entirely in the unchanged prefix was untouched by the edit,
    /// so its stored form is already correct whatever the index enumerates.
    /// Otherwise the identity moves onto its mapped position: if that position
    /// is occupied in the new index the reference is rewritten to it (a
    /// collision - what used to sit at the old offset now sits at its image,
    /// even when the old offset is occupied again by something else), and if
    /// it is not, the identity is lost.
    ///
    /// The one exception is an identity the file's own index still holds at
    /// its original position while the image is empty. Such an entry has not
    /// been rebuilt by the re-index yet, so it and the reference to it must
    /// stay in step rather than one being rewritten and the other not.
    fn resolve<T>(
        &self,
        mapped: Remap<T>,
        before_the_hunk: bool,
        original_occupied: bool,
        exists: impl Fn(&FileFacts, &T) -> bool,
    ) -> Remap<T> {
        let Some(facts) = &self.facts else {
            return mapped;
        };
        // `Unrelated` here means the identity is not in this file at all, so
        // the file's own facts say nothing about it.
        if matches!(mapped, Remap::Unrelated) || before_the_hunk {
            return mapped;
        }
        match &mapped {
            Remap::Moved(value) if exists(facts, value) => mapped,
            Remap::Moved(_) | Remap::Lost if original_occupied => Remap::Unrelated,
            Remap::Moved(_) | Remap::Lost => Remap::Lost,
            Remap::Unrelated => Remap::Unrelated,
        }
    }

    pub fn signature_id(&self, id: LuaSignatureId) -> Remap<LuaSignatureId> {
        let mapped = match self.pos(id.get_file_id(), id.get_position()) {
            Remap::Moved(position) => Remap::Moved(LuaSignatureId::new(id.get_file_id(), position)),
            Remap::Unrelated => Remap::Unrelated,
            Remap::Lost => Remap::Lost,
        };
        self.resolve(
            mapped,
            self.pos_is_before_the_hunk(id.get_position()),
            self.facts
                .as_ref()
                .is_some_and(|facts| facts.signature_positions.contains(&id.get_position())),
            |facts, id| facts.signature_positions.contains(&id.get_position()),
        )
    }

    pub fn decl_id(&self, id: LuaDeclId) -> Remap<LuaDeclId> {
        let mapped = match self.pos(id.file_id, id.position) {
            Remap::Moved(position) => Remap::Moved(LuaDeclId::new(id.file_id, position)),
            Remap::Unrelated => Remap::Unrelated,
            Remap::Lost => Remap::Lost,
        };
        self.resolve(
            mapped,
            self.pos_is_before_the_hunk(id.position),
            self.facts
                .as_ref()
                .is_some_and(|facts| facts.decl_positions.contains(&id.position)),
            |facts, id| facts.decl_positions.contains(&id.position),
        )
    }

    pub fn syntax_id(&self, file_id: FileId, id: LuaSyntaxId) -> Remap<LuaSyntaxId> {
        let mapped = match self.range(file_id, id.get_range()) {
            Remap::Moved(range) => Remap::Moved(with_range(id, range)),
            Remap::Unrelated => Remap::Unrelated,
            Remap::Lost => Remap::Lost,
        };
        self.resolve(
            mapped,
            self.range_is_before_the_hunk(id.get_range()),
            self.facts
                .as_ref()
                .is_some_and(|facts| facts.syntax_ids.contains(&id)),
            |facts, id| facts.syntax_ids.contains(id),
        )
    }

    pub fn member_id(&self, id: LuaMemberId) -> Remap<LuaMemberId> {
        let mapped = match self.range(id.file_id, id.get_syntax_id().get_range()) {
            Remap::Moved(range) => Remap::Moved(LuaMemberId::new(
                with_range(*id.get_syntax_id(), range),
                id.file_id,
            )),
            Remap::Unrelated => Remap::Unrelated,
            Remap::Lost => Remap::Lost,
        };
        self.resolve(
            mapped,
            self.range_is_before_the_hunk(id.get_syntax_id().get_range()),
            self.facts
                .as_ref()
                .is_some_and(|facts| facts.member_ids.contains(id.get_syntax_id())),
            |facts, id| facts.member_ids.contains(id.get_syntax_id()),
        )
    }

    /// A table literal's range, the identity behind `TableConst`, `Instance`
    /// and `LuaMemberOwner::Element`.
    pub fn table_range(&self, range: &InFiled<TextRange>) -> Remap<InFiled<TextRange>> {
        let mapped = match self.range(range.file_id, range.value) {
            Remap::Moved(value) => Remap::Moved(InFiled::new(range.file_id, value)),
            Remap::Unrelated => Remap::Unrelated,
            Remap::Lost => Remap::Lost,
        };
        self.resolve(
            mapped,
            self.range_is_before_the_hunk(range.value),
            self.facts
                .as_ref()
                .is_some_and(|facts| facts.owner_ranges.contains(&range.value)),
            |facts, range| facts.owner_ranges.contains(&range.value),
        )
    }

    pub fn type_owner(&self, owner: &LuaTypeOwner) -> Remap<LuaTypeOwner> {
        match owner {
            LuaTypeOwner::Decl(id) => map_into(self.decl_id(*id), LuaTypeOwner::Decl),
            LuaTypeOwner::Member(id) => map_into(self.member_id(*id), LuaTypeOwner::Member),
            LuaTypeOwner::SyntaxId(id) => map_into(self.syntax_id(id.file_id, id.value), |value| {
                LuaTypeOwner::SyntaxId(InFiled::new(id.file_id, value))
            }),
        }
    }

    pub fn definition_id(&self, id: &LuaDefinitionId) -> Remap<LuaDefinitionId> {
        match id {
            LuaDefinitionId::Declaration(decl_id) => {
                map_into(self.decl_id(*decl_id), LuaDefinitionId::Declaration)
            }
            LuaDefinitionId::Assignment {
                file_id,
                assignment,
                target_idx,
            } => map_into(self.syntax_id(*file_id, *assignment), |assignment| {
                LuaDefinitionId::Assignment {
                    file_id: *file_id,
                    assignment,
                    target_idx: *target_idx,
                }
            }),
        }
    }

    pub fn inference_node(&self, node: &LuaInferenceNodeId) -> Remap<LuaInferenceNodeId> {
        match node {
            LuaInferenceNodeId::TypeOwner(owner) => {
                map_into(self.type_owner(owner), LuaInferenceNodeId::TypeOwner)
            }
            LuaInferenceNodeId::Definition(definition) => map_into(
                self.definition_id(definition),
                LuaInferenceNodeId::Definition,
            ),
            LuaInferenceNodeId::SignatureParam {
                signature_id,
                param_idx,
            } => map_into(self.signature_id(*signature_id), |signature_id| {
                LuaInferenceNodeId::SignatureParam {
                    signature_id,
                    param_idx: *param_idx,
                }
            }),
        }
    }

    pub fn semantic_decl_id(&self, id: &LuaSemanticDeclId) -> Remap<LuaSemanticDeclId> {
        match id {
            // A type decl is named, not positioned.
            LuaSemanticDeclId::TypeDecl(_) => Remap::Unrelated,
            LuaSemanticDeclId::Member(member_id) => {
                map_into(self.member_id(*member_id), LuaSemanticDeclId::Member)
            }
            LuaSemanticDeclId::LuaDecl(decl_id) => {
                map_into(self.decl_id(*decl_id), LuaSemanticDeclId::LuaDecl)
            }
            LuaSemanticDeclId::Signature(signature_id) => map_into(
                self.signature_id(*signature_id),
                LuaSemanticDeclId::Signature,
            ),
        }
    }

    pub fn operator_owner(&self, owner: &LuaOperatorOwner) -> Remap<LuaOperatorOwner> {
        match owner {
            LuaOperatorOwner::Table(range) => {
                map_into(self.table_range(range), LuaOperatorOwner::Table)
            }
            LuaOperatorOwner::Type(_) => Remap::Unrelated,
        }
    }

    pub fn inferred_guard_owner(
        &self,
        owner: &LuaInferredGuardOwner,
    ) -> Remap<LuaInferredGuardOwner> {
        match owner {
            LuaInferredGuardOwner::GlobalPath {
                signature_id,
                state_mask,
                path,
            } => map_into(self.signature_id(*signature_id), |signature_id| {
                LuaInferredGuardOwner::GlobalPath {
                    signature_id,
                    state_mask: *state_mask,
                    path: path.clone(),
                }
            }),
        }
    }
}

fn map_into<T, U>(remap: Remap<T>, build: impl FnOnce(T) -> U) -> Remap<U> {
    match remap {
        Remap::Moved(value) => Remap::Moved(build(value)),
        Remap::Unrelated => Remap::Unrelated,
        Remap::Lost => Remap::Lost,
    }
}

/// The same node kind at a different range.
///
/// `LuaSyntaxId`'s kind field is private and its two accessors each read half
/// of it, so the round trip goes through whichever half `is_token` selects.
fn with_range(id: LuaSyntaxId, range: TextRange) -> LuaSyntaxId {
    let kind: LuaKind = if id.is_token() {
        id.get_token_kind().into()
    } else {
        id.get_kind().into()
    };
    LuaSyntaxId::new(kind, range)
}
